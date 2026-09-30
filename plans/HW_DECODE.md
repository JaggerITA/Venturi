# HW_DECODE — hardware video decoding

Follow-up to Vikunja #25 (black viewer after repositioning during
playback on heavy footage). One code path over FFmpeg's hwaccel API for
every platform: VideoToolbox on macOS, NVDEC and Vulkan video on Linux.

**Status:** in progress (see §7).

---

## 1. What is already done, and what HW decode is for

The #25 black came from the transit (keyframe → playhead) more than from
raw decode speed. On `master`:

- `a5efc87` no scaler when the decoded frame is already YUV420P
  (decoder 185 → 400 fps on the #25 clip);
- `5ea15d4` the transit after a seek is not converted nor cached, and its
  non-reference frames are not decoded (`Decoder::skip_before`);
- `e3f1e06` a jump of the user abandons the decode in progress;
- `314b460` no fake frames after a mid-stream skip.

The #25 clip (HEVC Main 4516×1080 @ 60, MKV, 250-frame GOP) now shows its
first frame 130–540 ms after a click (`bench_black_frames_after_repositioning_during_playback`).

Measured on the dev machine (24 threads, RTX 2070 Super), 1200 frames
of that clip:

| Decoder | fps | CPU time |
|---|---|---|
| software, 24 threads | 505 | 23.6 s |
| NVDEC | 475 | 1.2 s |
| Vulkan video (NVIDIA driver) | 365 | 1.7 s |
| VA-API (NVIDIA → VA-API shim) | 264 | 2.5 s |

**Caveat:** it is not known whether these numbers include downloading
the frames to system memory. Every HW benchmark of this plan measures
decode + transfer + packing into `FrameYuv420`, and reports the transfer
time separately: the readback, not the decode, can be the bottleneck.

Remeasured that way (`bench_hw_decode` in `tests/decode.rs`, same clip,
release build, 2026-09-30, still converting NV12 → YUV420P with sws):

| Decoder | open + first frame | fps | transfer / frame | CPU time |
|---|---|---|---|---|
| software, 24 threads | 125 ms | 368 | — | 26.8 s |
| NVDEC | 57 ms | 202 | 1.9 ms | 3.9 s |
| NVDEC, frame threads on | 99 ms | 252 | 1.8 ms | 3.9 s |
| Vulkan (NVIDIA driver) | 27–35 ms | 88–168 (noisy) | 3.9–5.5 ms | 3.9–8.6 s |

- The transfer is a third to a half of the HW time per frame: the
  earlier table did not include it.
- `av_hwframe_map` on Vulkan was no faster than a transfer (CUDA cannot
  map): only VideoToolbox frames are mapped.
- Frame threading with NVDEC overlaps decode and transfer (+25%) but
  slows the open; left off for now, to remeasure after §4.6 removes sws.

After §4.6 (NV12 kept as it is, no sws):

| Decoder | open + first frame | fps | transfer / frame | CPU time |
|---|---|---|---|---|
| software, 24 threads | 126 ms | 382 | — | 26.1 s |
| NVDEC | 52 ms | 265 | 1.8 ms | 2.6 s |
| Vulkan (NVIDIA driver) | 29 ms | 135 | 4.6 ms | 5.7 s |

With frame threads on, two runs gave NVDEC 323 and 140 fps (transfer
1.8 vs 4.7 ms): too noisy to decide on this machine. Still off; to
measure on a quiet machine and on the M1.

The dev box is atypical (24-thread CPU): on most workstations the GPU
decoder beats the CPU, so HW decode is the default (`Auto`). Where it
only frees the CPU it still helps export, UI and audio running in
parallel. The target machine to validate latency on is a MacBook Air M1
with 8 GB, where VideoToolbox is far faster than software HEVC.

## 2. Constraints

- Software stays the fallback everywhere. No V4L2: ARCHITECTURE.md rules
  it out on Asahi (unstable AVD decoder).
- No VA-API: FFmpeg links libva at link time, so an AppImage built with
  it would not even start on a system without libva. NVDEC (ffnvcodec)
  and Vulkan are loaded with `dlopen`; the AppImage CI checks with
  `readelf -d` that no `libva*`/`libcuda*`/`libvulkan*` is `NEEDED`.
  Intel/AMD on Linux go through Vulkan video (Mesa ANV/RADV).
- Any failure falls back to software for that media, remembered: no
  device, codec/profile/size unsupported (e.g. H.264 4:2:2, H.264 wider
  than 4096 on NVDEC), error mid-stream.
- The transit machinery (`skip_before`, `landing`, `skip_some`) must
  work unchanged on a HW decoder: it only relies on pts and
  `skip_frame`, which hwaccels honour.

## 3. Backends

| Platform | Backend (`AVHWDeviceType`) | Build (`scripts/build-ffmpeg.sh`) |
|---|---|---|
| macOS | VideoToolbox | already `--enable-videotoolbox` |
| Linux NVIDIA | CUDA (NVDEC) | `--enable-nvdec` (nv-codec-headers already installed) |
| Linux, any vendor | Vulkan | `--enable-vulkan` (Vulkan headers in the CI image, the version FFmpeg `n9.0.1` requires) |
| Windows | D3D11VA | no Windows build today: only leave room |
| Raspberry Pi | — | out of scope: Pi 4 H.264 is a separate `h264_v4l2m2m` decoder, HEVC needs the V4L2 request API only in the Raspberry Pi FFmpeg fork, Pi 5 has no H.264 HW |

`Auto` order:

- macOS: VideoToolbox.
- Linux: if an integrated GPU exposes Vulkan video decode and a discrete
  one is also present (hybrid laptop), Vulkan on the integrated GPU: NVDEC
  would wake the discrete GPU and cost battery. Otherwise NVDEC if a CUDA
  device opens, else Vulkan on the first device with a video decode
  queue.

Dev builds link the system FFmpeg: backends are probed at run time,
never assumed.

## 4. Design

### 4.1 One module, one path — `vv-media/src/hw.rs`

```rust
pub enum HwBackend { VideoToolbox, Cuda, Vulkan }
impl HwBackend {
    fn device_type(self) -> AVHWDeviceType;
    fn candidates() -> &'static [HwBackend]; // per target_os
}
```

Everything else is shared by all backends:

1. **Device.** One `AVBufferRef` per (backend, GPU) per process, created
   lazily in a `OnceLock` and shared (`av_buffer_ref`) by every decoder.
   A failed creation is memoized. Vulkan picks the GPU explicitly (device
   index/name), never the driver's default. CUDA init costs 100–300 ms:
   warm it up on a background thread at startup, like the NVENC check.
2. **Codec setup.** `avcodec_get_hw_config` → a config with
   `HW_DEVICE_CTX` for the device type; set `hw_device_ctx`, a
   `get_format` callback (raw `extern "C"`, ffmpeg-next does not wrap it)
   returning the HW format if offered, else the first software one;
   `extra_hw_frames` for the frames the worker holds (`pending`). Frame
   threading off with a hwaccel.
3. **Frames.** `Decoder::convert` gets a HW frame: `av_hwframe_map`
   where the backend supports it (VideoToolbox, Vulkan), else
   `av_hwframe_transfer_data`, into NV12 (P010 for 10-bit), then
   `av_frame_copy_props` for pts and colour metadata. `pack_plane`
   already copies row by row: mapping saves one full-frame copy.
4. **Lazy scaler.** Today the `Scaler` is built in `open` from
   `decoder.format()`. With a hwaccel that is the stream's declared
   format (yuv420p), while frames arrive as NV12/P010 after the transfer,
   and `Scaler::run` refuses an input of another format (`InputChanged`).
   The scaler is built on the first frame that needs one, keyed by that
   frame's format, and rebuilt if the format changes (useful in software
   too).

### 4.2 Fallback

FFmpeg falls back **silently**: when the hwaccel fails to initialise
(profile, chroma format or size unsupported) libavcodec calls
`get_format` again without the HW format, our callback returns the
software one, and decoding goes on with no error. With frame threading
already off (it cannot be turned on after open), that is HEVC 4.5K on a
single thread, ~10× slower, while the decoder believes it is on HW.

So HW is confirmed by the frames, not by the setup:

- `seek_to_time` decodes the landing frame anyway: if its format is not
  the HW one, or the decode errors, the decoder reopens in full software
  (frame threads on) and the media goes into a per-path "HW failed" set,
  so it is not retried.
- `Decoder::is_hw()` reports what the frames say.
- **Error mid-stream** (from `next_frame`, or during a transit inside
  `skip_some`): reopen in software, seek to the last emitted index + 1,
  and restore the decoder's position state as the caller left it:
  `skip_before` if a transit was in progress, `emit_idx`/`held` cleared
  as after a seek (else the reopen fills the gap with copies of the last
  frame, the bug `314b460` fixed). Done once per media; a test injects
  the error.

### 4.3 Memory budget for HW surfaces

Each HW decoder allocates its own surface pool: the DPB (up to ~16
frames for HEVC) plus `extra_hw_frames`. At 4.5K NV12 that is ~7 MB a
frame, 100+ MB per decoder, and `render_ahead` keeps **two** decoders
per media (`open` + `open_behind`) for several media at once. On Apple
Silicon it is the same unified memory as the frame cache.

- Budget `hw_decode_budget_bytes`, in Settings > Playback next to the
  frame cache budget; default proportional to physical RAM (1/8,
  `sysconf(_SC_PHYS_PAGES)` works on Linux and macOS).
- Each HW decoder charges its estimate (surfaces × frame bytes) on open
  and releases it on drop. Over budget, the new decoder opens in
  software.
- Priority: forward decoders first. `open_behind` goes HW only if the
  budget still allows after the forward ones.

### 4.4 API

`Decoder::open` stays software (thumbnails, probe: device init would
dominate). `Decoder::open_with(path, HwDecode)` for render-ahead,
proxies and, after a check, export.

### 4.5 Reopen cost — measure first

`walk_and_fill` drops (`open.retain`) the decoders of media that leave
the window, and they are reopened when they come back. With HW each
reopen redoes `hw_frames_ctx` and the VideoToolbox/NVDEC session (tens
of ms?), on top of the container parse already paid today. The shared
device (§4.1.1) does not cover this.

Measured (§1): open + first frame is *shorter* on HW (27–57 ms) than in
software (125 ms, which spins up 24 frame threads), with the device
already created. No LRU needed; the CUDA device must be warmed up at
startup, as planned. To recheck on the M1.

### 4.6 NV12 end to end

Transferred frames are NV12 (chroma interleaved). Converting them to
YUV420P with sws puts back the ~3 ms per 4.5K frame that `a5efc87`
removed, so NV12 has to travel as it is:

- `FrameYuv420` gets its chroma as an enum: planar `u`/`v` (today) or
  interleaved `uv`. `byte_len` unchanged (same size).
- `vv-render/src/compositor.rs` uploads the interleaved plane as one
  `Rg8Unorm` texture into the `u` slot, with a 1×1 placeholder in the `v`
  slot (as already done for `alpha`); a flag in the per-layer uniform
  makes `shaders/transform.wgsl` sample `.rg` from `u`. Same bind layout,
  no second set of pipelines.
- CPU consumers of decoded frames:
  - `thumbnail.rs` reads the planes: it learns NV12 (or `to_planar()`).
  - `proxy.rs` `write_frame` already goes through a scaler: it declares
    the source as NV12, no `to_planar()`.
  - The export encoder (`encode.rs`) is **not** one: it encodes the
    compositor's output (`rgba_to_i420.wgsl`), never a decoded frame.
- Software decoding of NV12 sources (some cameras, screen recorders) also
  benefits: no sws for them either.
- P010 (10-bit): convert to 8-bit NV12 for now; a 16-bit texture path
  is a separate topic. It costs what the 10-bit software path costs
  today, but on the M1 10-bit HEVC (iPhone HLG) is the common case:
  measure it separately.

### 4.7 Zero-copy — later, maybe

Keeping frames on the GPU (VideoToolbox IOSurface → Metal, CUDA/Vulkan →
wgpu Vulkan) skips transfer, cache RAM and upload. It is the largest win
at 4K+, but each platform needs its own interop through wgpu-hal: the
only part of this plan with per-platform code. Only if the numbers after
§4.6 justify it, starting with VideoToolbox.

## 5. Setting

In Settings > Playback (`settings_dialog.rs`, `it:` translation in
`crates/vv-app/locales/`):

- `Settings::hw_decode: Auto | Off | VideoToolbox | Nvdec | Vulkan`
  (only the backends of the platform are listed), default `Auto`.
  Explicit choice is how to force NVDEC on a hybrid laptop, or Vulkan on
  NVIDIA.
- `hw_decode_budget_bytes` (§4.3).

A change goes to the worker like `SetProxy`: drop decoders, keep the
cache (same pixels).

## 6. Verification

- `crates/vv-media/src/tests/decode.rs` (not inline, see CLAUDE.md):
  - `open_with(Auto)` without a device decodes the same frames as
    `open` (runs in CI);
  - a hwaccel that silently falls back is detected (format of the first
    frame) and the decoder ends up in software with frame threads;
  - an injected mid-stream error reopens in software and hands out the
    same sequence as an uninterrupted decode, also during a transit;
  - `#[ignore]`: HW vs software frames within a small tolerance, and
    `skip_before` on a HW decoder hands out the same frames as a full
    decode.
- Compositor: an NV12 frame and the same frame as planar render
  identical pixels (CI has lavapipe).
- Numbers, per machine (dev box, MacBook Air M1), always with the
  transfer included and reported separately (§1):
  - decode throughput, software vs each HW backend;
  - open + first frame, software vs HW (§4.5);
  - 10-bit HEVC on the M1;
  - `bench_black_frames_after_repositioning_during_playback` with
    `VV_BENCH_CLIP`.
- Manual: #25 scenario with proxies off; export and proxy generation
  still byte-identical in software mode.

## 7. Order

1. ~~Build flags (§3), `readelf` check on the AppImage~~ done (the Debian
   13 image already has Vulkan headers 1.4.309).
2. ~~Lazy scaler (§4.1.4)~~ done.
3. ~~`hw.rs`: device, codec setup, transfer/map, fallback (§4.2), with
   tests~~ done.
4. ~~Benchmarks with transfer and reopen cost~~ done on the dev box (§1).
5. ~~NV12 in `FrameYuv420`, compositor and shader, CPU consumers~~ done
   (`FrameYuv420::chroma_at` for the thumbnails).
6. ~~Budget (§4.3), wiring into render-ahead, then proxies; settings and
   translation~~ done. `bench_black_frames_after_repositioning_during_playback`
   on a 180 s clip like #25's (the bench needs ≥ 137 s, it clicks up to
   frame 8180), `VV_BENCH_HW` to pick the backend, 2026-09-30:

   | | first frame after a click | black refreshes / 180 |
   |---|---|---|
   | software | 184–636 ms | 11–38 |
   | Auto (NVDEC) | 67–284 ms | 4–17 |
   | Vulkan | 117–335 ms | 7–20 |

   HW halves the #25 latency even on the dev box: the transit's skipped
   frames are decoded but never transferred.
7. Measure on the dev box and on the M1; then decide on export and on
   zero-copy.
