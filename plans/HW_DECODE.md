# HW_DECODE — hardware video decoding

Follow-up to Vikunja #25 (black viewer after repositioning during
playback on heavy footage). One code path over FFmpeg's hwaccel API for
every platform: VideoToolbox on macOS, VA-API (Intel/AMD) and NVDEC on
Linux, Vulkan video as an opt-in.

**Status:** proposed, not started.

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

So on a strong desktop HW decode does **not** shorten #25: it frees the
CPU (export, UI and audio in parallel, battery). The win in latency is
where the CPU is weak: laptops, and Apple Silicon, where VideoToolbox is
far faster than software HEVC. The target machine to validate on is a
MacBook Air M1 with 8 GB.

## 2. Constraints

- Opt-in per backend, software stays the fallback everywhere. No V4L2:
  ARCHITECTURE.md rules it out on Asahi (unstable AVD decoder).
- Any failure falls back to software for that media, remembered: no
  device, codec/profile/size unsupported (e.g. H.264 4:2:2, H.264 wider
  than 4096 on NVDEC and most VA-API drivers), error mid-stream.
- The transit machinery (`skip_before`, `landing`, `skip_some`) must
  work unchanged on a HW decoder: it only relies on pts and
  `skip_frame`, which hwaccels honour.

## 3. Backends

| Platform | Backend (`AVHWDeviceType`) | Build (`scripts/build-ffmpeg.sh`) |
|---|---|---|
| macOS | VideoToolbox | already `--enable-videotoolbox` |
| Linux Intel/AMD | VA-API | `--enable-vaapi`, libva headers in the CI image; libva not bundled in the AppImage (tied to the system driver) |
| Linux NVIDIA | CUDA (NVDEC) | `--enable-nvdec` (nv-codec-headers already installed) |
| Linux, any vendor | Vulkan | `--enable-vulkan`; opt-in: younger, slower than NVDEC here |
| Windows | D3D11VA | no Windows build today: only leave room |
| Raspberry Pi | — | out of scope: Pi 4 H.264 is a separate `h264_v4l2m2m` decoder, HEVC needs the V4L2 request API only in the Raspberry Pi FFmpeg fork, Pi 5 has no H.264 HW |

Auto order: macOS VideoToolbox; Linux NVDEC if a CUDA device opens, else
VA-API. Vulkan only when chosen explicitly. Dev builds link the system
FFmpeg: backends are probed at run time, never assumed.

## 4. Design

### 4.1 One module, one path — `vv-media/src/hw.rs`

```rust
pub enum HwBackend { VideoToolbox, Vaapi, Cuda, Vulkan }
impl HwBackend {
    fn device_type(self) -> AVHWDeviceType;
    fn candidates() -> &'static [HwBackend]; // per target_os
}
```

Everything else is shared by all backends:

1. **Device.** One `AVBufferRef` per backend per process, created lazily
   in a `OnceLock` and shared (`av_buffer_ref`) by every decoder. A
   failed creation is memoized. CUDA init costs 100–300 ms: warm it up on
   a background thread at startup, like the NVENC check.
2. **Codec setup.** `avcodec_get_hw_config` → a config with
   `HW_DEVICE_CTX` for the device type; set `hw_device_ctx`, a
   `get_format` callback (raw `extern "C"`, ffmpeg-next does not wrap it)
   returning the HW format if offered, else the first software one;
   `extra_hw_frames` for the frames the worker holds. Frame threading off
   with a hwaccel.
3. **Frames.** `Decoder::convert` gets a HW frame: `av_hwframe_transfer_data`
   into a software frame (NV12, or P010 for 10-bit),
   `av_frame_copy_props` for pts and colour metadata.
4. **Fallback.** Setup failure, or an error on the frame `seek_to_time`
   decodes anyway → reopen in software, `Decoder::is_hw()` tells which.
   An error later mid-stream: reopen in software and seek back, once;
   a per-path set of "HW failed" media avoids retrying.
5. **API.** `Decoder::open` stays software (thumbnails, probe: device
   init would dominate). `Decoder::open_with(path, HwDecode)` for
   render-ahead, proxies and, after a check, export.

### 4.2 NV12 end to end

Transferred frames are NV12 (chroma interleaved). Converting them to
YUV420P with sws puts back the ~3 ms per 4.5K frame that `a5efc87`
removed, so NV12 has to travel as it is:

- `FrameYuv420` gets its chroma as an enum: planar `u`/`v` (today) or
  interleaved `uv`. `byte_len` unchanged (same size).
- `vv-render/src/compositor.rs` uploads the interleaved plane as one
  `Rg8Unorm` texture; `shaders/transform.wgsl` samples `.rg` instead of
  two `R8Unorm` planes (a flag in the per-layer uniform, or a second bind
  layout).
- Consumers that read the planes on the CPU (`thumbnail.rs`, `proxy.rs`
  and `encode.rs` feeding x264, which accepts NV12 directly) get a
  `to_planar()` helper or take NV12 as is.
- Software decoding of NV12 sources (some cameras, screen recorders) also
  benefits: no sws for them either.
- P010 (10-bit): convert to 8-bit NV12 for now; a 16-bit texture path
  is a separate topic.

### 4.3 Zero-copy — later, maybe

Keeping frames on the GPU (VideoToolbox IOSurface → Metal, VA-API
DMA-BUF → Vulkan, CUDA → Vulkan) skips transfer, cache RAM and upload.
It is the largest win at 4K+, but each platform needs its own interop
through wgpu-hal: the only part of this plan with per-platform code. Only
if the numbers after §4.2 justify it, starting with VideoToolbox.

## 5. Setting

`Settings::hw_decode: Auto | Off` in Settings > Playback
(`settings_dialog.rs`, `it:` translation in `crates/vv-app/locales/`).
A change goes to the worker like `SetProxy`: drop decoders, keep the
cache (same pixels).

## 6. Verification

- `crates/vv-media/src/tests/decode.rs` (not inline, see CLAUDE.md):
  - `open_with(Auto)` without a device decodes the same frames as
    `open` (runs in CI);
  - `#[ignore]`: HW vs software frames within a small tolerance, and
    `skip_before` on a HW decoder hands out the same frames as a full
    decode.
- Compositor: an NV12 frame and the same frame as planar render
  identical pixels (CI has lavapipe).
- Numbers, per machine (dev box, MacBook Air M1):
  - a decode throughput benchmark, software vs HW;
  - `bench_black_frames_after_repositioning_during_playback` with
    `VV_BENCH_CLIP`.
- Manual: #25 scenario with proxies off; export and proxy generation
  still byte-identical in software mode.

## 7. Order

1. Build flags (§3) and CI image.
2. `hw.rs`: device, codec setup, transfer, fallback, with tests.
3. NV12 in `FrameYuv420`, compositor and shader, CPU consumers.
4. Wire into render-ahead, then proxies; setting and translation.
5. Measure on the dev box and on the M1; then decide on export and on
   zero-copy.
