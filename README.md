# Venturi

A fast, focused video editor for Linux, written in Rust.

Venturi is an "edit page only" NLE: multi-track cutting, the transforms you
actually reach for while editing (crop, zoom, rotation, position, speed,
opacity, audio gain, titles, solid colours), keyframes on every parameter,
transitions, compound clips, ripple/normal delete. No node editor, no colour
grading page, no fusion-style compositor. The scope is deliberate: the edit
page is where the time goes, so that is the part that gets to be excellent.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design.

## Why another editor

Venturi started from a simple frustration: on a modern laptop, cutting 1080p
H.264 footage should feel instant, and usually it does not. Most editors treat
the timeline as a collection of clips, each with its own decoder, its own
cache, its own idea of what to keep in memory. Scrub across a cut and
everything starts over. Group a few clips and playback falls off a cliff.

Venturi is built the other way around.

### The timeline is the unit, not the clip

There is one frame cache for the whole project, with a single global byte
budget (1.2 GB by default, configurable). A single worker thread walks the
timeline forward from the playhead — through cuts, gaps, and every video
track at once — and fills it. Crossing a cut is not a special case; it is just
the next frame.

Eviction is not LRU. During a forward fill the frame *at the playhead* is the
oldest one inserted, so any recency-based policy would throw away exactly the
frame you are about to need. Instead a single reconcile pass drops whatever
falls outside the current window, then evicts by **distance from the playhead
in timeline frames**. The frame you need next is the last one to go.

### Nothing is baked

Transforms are evaluated at runtime from their keyframes and handed to the
shader as uniforms — never baked into cached frames. Dragging a crop slider
does not invalidate a single buffered frame. The same applies to the
audio gain curve, which is sampled per block inside the mixer.

### No readback in the preview path

The compositor is wgpu on Vulkan, sharing its device with the egui UI. The
preview composes straight into a texture that stays on the GPU and is handed
to `egui-wgpu` as-is — no `copy_texture_to_buffer`, no `map_read`, no
pipeline stall per frame. Compound clips compose into a pooled intermediate
texture that is sampled directly by the outer pass, instead of making a round
trip through RGB↔YUV on the CPU.

Export uses the same code path with a different tail: decode, GPU compositing
and encode run as three pipelined threads, and the RGBA→I420 conversion is a
compute shader, so the readback is half the size and happens once per frame
instead of twice.

### The audio clock is the playback clock

The mixer's position, in timeline samples, *is* the playhead; the video
chases it. A gap is silence and the clock keeps running. The `cpal` callback
mixes from an immutable snapshot published by the UI thread with a
non-blocking `try_lock` — no allocation, no blocking lock, and old snapshots
are dropped on the UI thread so a deallocation can never land in the audio
callback. Fast-forward (2×/4×/8×) renders 8-second windows of the mix,
pitch-preserved through `rubberband`, in the background and queues them
without ever reopening the stream.

### Scrubbing gets its own format

Long-GOP H.264 cannot be scrubbed: every frame needs a decode from the
previous keyframe. So every imported file gets a low-resolution all-intra
proxy generated in the background, keyed by a content fingerprint in a global
cache — it is reused by every project that touches the same file, even before
you save. Preview uses it; **export always reads the originals**.

### Correctness the same way

Fast is only worth something if the frames are right. A clip whose media runs
at a different rate than the timeline is *conformed*: `Clip::rate` is the one
place where the two frame spaces meet, so a 59.94 clip on a 60 fps timeline
occupies its real duration and duplicates a frame roughly every 17 seconds
instead of drifting out of sync with its own audio. Splits and trims land
exactly where you put them, even mid source frame. Preview and export share
one `mix_range` for audio and one layer-building path for video, so what you
hear and see while editing is what gets written out.

### The rest of the shape

- **Data-oriented model, no node graph.** A single `Project` owned by the UI
  thread; media and timelines in `slotmap` arenas, clips in time-sorted
  `Vec`s. Workers get snapshots or copies.
- **Undo/redo by command pattern.** Every command captures what it needs to
  invert itself. The history is light and unbounded.
- **Projects are RON.** Readable, diffable, greppable, reviewable in git.
- **Rust, no GC.** No pause is ever someone else's decision.
- **Immediate-mode UI (egui).** The timeline is painted, not built from
  nested widgets — cheaper for a dense grid of rectangles, and it shares the
  wgpu device with the compositor.
- **OpenTimelineIO in and out**, so a project can leave.

Primary development and testing happens on Fedora Asahi Remix (Apple
Silicon), but nothing here depends on Asahi: building on other Linux distros
should work the same way, given the system libraries below.

## Dependencies

- **Rust** 1.85 or newer (edition 2024) — [rustup.rs](https://rustup.rs)
- **FFmpeg** (development headers/libs + `pkg-config`, for `ffmpeg-next`, the
  binding used for decode/encode)
- **clang/libclang** (for `ffmpeg-next`'s bindgen)
- **Wayland/X11 + Vulkan** (for `eframe`/`wgpu`, the UI and the GPU compositor)
- **ALSA** (for `cpal`, the audio)

On Fedora (including Asahi Remix):

```sh
sudo dnf install \
  rust cargo \
  ffmpeg ffmpeg-devel \
  clang clang-devel \
  wayland-devel libxkbcommon-devel libxkbcommon-x11 libX11-devel \
  vulkan-loader-devel mesa-vulkan-drivers \
  alsa-lib-devel
```

`ffmpeg-devel` on Fedora requires the RPM Fusion (free) repository to be
enabled — Fedora's own system build of FFmpeg does not include libx264 for
licensing reasons, but this project needs it both for decoding common H.264
sources and for export/proxies.

On Debian/Ubuntu the equivalents (not verified in this session, only
translated from the Fedora packages above):

```sh
sudo apt install \
  build-essential pkg-config \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev \
  clang libclang-dev \
  libwayland-dev libxkbcommon-dev libx11-dev \
  libvulkan-dev mesa-vulkan-drivers \
  libasound2-dev
```

### libav version

If your system has a different version than the one linked against, e.g.:

```
error while loading shared libraries: libavutil.so.60: cannot open shared object file: No such file or directory
```

you can force a specific `libavutil` version through the `LD_LIBRARY_PATH`
environment variable, e.g.:

```
export LD_LIBRARY_PATH="/path/to/local/library/:$LD_LIBRARY_PATH"
./vv-app
```

to use a different local version.

## Build

From the workspace root:

```sh
cargo build -p vv-app            # debug
cargo build -p vv-app --release  # release (optimised, much slower to compile)
```

The first build is slow (`ffmpeg-next`'s bindgen + compiling `wgpu`);
subsequent ones are incremental.

### AppImage

```sh
scripts/build-appimage.sh
```

Produces `target/appimage/Venturi-<arch>.AppImage` with FFmpeg compiled from
source inside it (shared libraries, with libx264, librubberband, zlib and
NVENC) plus libx264: the target machine needs neither FFmpeg nor RPM Fusion.
glibc, ALSA, Vulkan and the NVIDIA driver, if any (NVENC requires >= 550),
stay the system ones.

The script re-runs itself inside a Debian 13 container
(`container/Containerfile.appimage`, built on first use): a glibc binary only
runs on a glibc >= the build one, so the release must not be compiled on
Fedora. All the host needs is `podman`.

FFmpeg is compiled once inside the build volume
(`venturi-appimage-target`); to start over from scratch:
`podman volume rm venturi-appimage-cargo venturi-appimage-target`,
and `podman rmi venturi-appimage` if you change `Containerfile.appimage`.

The `AppImage build` GitHub Actions workflow (`.github/workflows/appimage.yml`)
builds the same image and runs the same script on x86_64 and aarch64 runners,
manually or on `v*` tags, and uploads the AppImages as artifacts.

The AppImage is GPL (it includes libx264) and does not include FDK-AAC:
export uses FFmpeg's native AAC encoder.

### macOS (Apple Silicon)

```sh
scripts/build-macos.sh
```

Must run on a Mac with the Xcode command line tools and `pkgconf`. Produces
`target/macos/Venturi.app` and `target/macos/Venturi-arm64.dmg`, with FFmpeg
(libx264, librubberband, zlib, VideoToolbox) bundled in
`Contents/Frameworks`.

The `macOS build` GitHub Actions workflow (`.github/workflows/macos.yml`) runs
the same script on a `macos-14` runner, manually or on `v*` tags, and uploads
the dmg as an artifact.

#### Opening the app on another Mac

The app is signed ad-hoc only, not notarized: once downloaded (browser,
AirDrop, Nextcloud…) macOS quarantines it and refuses to launch it, with
"The application Venturi can't be opened" or, from the terminal,
`zsh: operation not permitted`. Distribute the `.dmg`, not the bare `.app`
folder: zips and FAT/exFAT drives can drop the executable bit.

1. Remove the quarantine and launch:
   ```sh
   xattr -dr com.apple.quarantine /Applications/Venturi.app
   open /Applications/Venturi.app
   ```
2. If `xattr` also answers `operation not permitted`: System Settings →
   Privacy & Security → App Management, enable Terminal and repeat step 1.
   Alternatively, copy the app to the Desktop, remove the quarantine there and
   move it to `/Applications` afterwards.
3. Without a terminal: try to open the app once, then System Settings →
   Privacy & Security → "Open Anyway" at the bottom. Right click → Open no
   longer bypasses Gatekeeper since macOS Sequoia.

If it still does not start:
```sh
ls -l /Applications/Venturi.app/Contents/MacOS/vv-app   # needs the x bit
codesign -vvv --deep /Applications/Venturi.app          # signature intact?
/Applications/Venturi.app/Contents/MacOS/vv-app         # real startup error
```
A missing `x` is fixed with `chmod +x` on that file, a broken signature with
`codesign --force --deep --sign - /Applications/Venturi.app`.

## Run

```sh
cargo run -p vv-app
```

A working graphics backend is required at runtime (Vulkan on Linux, through
`mesa-vulkan-drivers` or the proprietary GPU driver): without one, `wgpu`
finds no adapter and the window does not open.

## Test

```sh
cargo test -p vv-app -- --test-threads=1
```

`--test-threads=1` is not optional for now: there is an intermittent flake
(SIGSEGV, not yet investigated) when the tests run in parallel — single
threaded they are stable. Some tests invoke `ffmpeg` from the command line to
generate synthetic clips in `/tmp`, so the `ffmpeg` binary (not just the
development libraries) also needs to be in `PATH`.

## Lint

```sh
cargo clippy --all-targets
cargo fmt --check
```

## Headless test container

[`container/`](container/README.md) contains a Podman environment to
build/run/screenshot vv-app without a real graphical session (Xvfb + software
Vulkan) — useful for checking the UI in isolation, or from an agent without
access to the user's display.

## Licence

GPL-3.0-or-later, see [LICENSE](LICENSE).
