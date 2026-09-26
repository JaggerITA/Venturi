# Test container (headless)

Podman environment to build/run/screenshot vv-app without a real graphical
session — Xvfb as a virtual X server, software Vulkan (lavapipe, already in
`mesa-vulkan-drivers`) for `wgpu` instead of requiring a real GPU passed to
the container. The repo is not inside the image: it is mounted at runtime,
so it always reflects the current local state without rebuilding the image
on every change.

## Setup (once)

```sh
podman build -t venturi-test -f container/Containerfile container/
```

## Quick use (single command, throwaway container)

```sh
container/run.sh cargo build -p vv-app
container/run.sh cargo test -p vv-app -- --test-threads=1
container/run.sh bash   # interactive shell
```

## AppImage build

Uses a separate image (`Containerfile.appimage`, Debian 13) because the
release must link against an old glibc, not Fedora 44's:

```sh
scripts/build-appimage.sh   # -> target/appimage/Venturi-<arch>.AppImage
```

The script re-enters the container by itself (through
`container/build-appimage.sh`, which builds the image on first use). See
../docs/BUILDING.md for the cache volumes.

## Testing the UI (persistent session)

A single command (`run.sh`) is not enough for "start the app, interact,
screenshot, interact again" — that needs a container that stays alive
between one command and the next:

```sh
container/session.sh start                      # starts Xvfb + the container
container/session.sh exec cargo build -p vv-app

# Start the app in the background inside the session (the binary is in
# /cargo-target/debug/, not in $PATH — see CARGO_TARGET_DIR in the
# Containerfile):
container/session.sh exec bash -c \
    'nohup /cargo-target/debug/vv-app >/tmp/vv-app.log 2>&1 & disown'

container/session.sh shot startup                # -> container/shots/startup.png
container/session.sh exec xdotool mousemove 162 11 click 1   # opens a menu
container/session.sh shot menu-open

container/session.sh exec pkill -f vv-app
container/session.sh stop                        # stops and removes the container
```

`xdotool` takes absolute coordinates on the virtual screen (1280x800 by
default, `XVFB_RESOLUTION` in the Containerfile) — use a previous screenshot
to eyeball the coordinates of the next click. Useful commands:
`mousemove X Y`, `click 1`/`click 3` (left/right button), `key <name>`
(e.g. `key space`), `keydown`/`keyup`, and for a drag
(`mousedown 1` ... `mousemove` ... `mouseup 1`).

## Cache between runs

`run.sh`/`session.sh` mount two named Podman volumes
(`venturi-cargo-registry`, `venturi-target`) so downloaded dependencies and
build artifacts persist from one container to the next — only the first
build is slow (`ffmpeg-next` bindgen + `wgpu`, ~2 minutes), later ones are
incremental. To start over from scratch:
`podman volume rm venturi-cargo-registry venturi-target`.

## Known limitations

- No real audio (`/etc/asound.conf` points to a null ALSA device): enough
  for `cpal` not to fail opening the stream, not to hear anything —
  irrelevant for visual tests of the timeline/UI.
- Software rendering (lavapipe): functionally correct but slower than a
  real GPU — fine for a screenshot or an interaction round, not for
  measuring framerate/perceived performance.
- No window manager: windows have no decorations and there is no
  focus-follows-mouse — `xdotool` interacts through absolute coordinates
  anyway, but actions that assume a WM (e.g. alt-tab) do not apply.
