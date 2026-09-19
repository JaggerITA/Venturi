# vibevideo

Editor video "solo edit page" in Rust — vedi [ARCHITECTURE.md](ARCHITECTURE.md)
per il design. Sviluppato e testato principalmente su Fedora Asahi Remix
(Apple Silicon), ma non dipende da nulla di specifico ad Asahi: build su
altre distro Linux dovrebbe funzionare allo stesso modo, a patto delle
librerie di sistema sotto.

## Dipendenze

- **Rust** 1.85 o più recente (edition 2024) — [rustup.rs](https://rustup.rs)
- **FFmpeg** (header/lib di sviluppo + `pkg-config`, per `ffmpeg-next`, il
  binding usato per decode/encode)
- **clang/libclang** (per il bindgen di `ffmpeg-next`)
- **Wayland/X11 + Vulkan** (per `eframe`/`wgpu`, la UI e il compositor GPU)
- **ALSA** (per `cpal`, l'audio)

Su Fedora (compreso Asahi Remix):

```sh
sudo dnf install \
  rust cargo \
  ffmpeg ffmpeg-devel \
  clang clang-devel \
  wayland-devel libxkbcommon-devel libxkbcommon-x11 libX11-devel \
  vulkan-loader-devel mesa-vulkan-drivers \
  alsa-lib-devel
```

`ffmpeg-devel` su Fedora richiede il repo RPM Fusion (free) abilitato — la
build di sistema di FFmpeg di Fedora stesso non include libx264 per motivi
di licenza, ma questo progetto lo richiede sia per il decode di sorgenti
H.264 comuni sia per l'export/i proxy.

Su Debian/Ubuntu gli equivalenti (non verificati in questa sessione, solo
tradotti dai pacchetti Fedora sopra):

```sh
sudo apt install \
  build-essential pkg-config \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev \
  clang libclang-dev \
  libwayland-dev libxkbcommon-dev libx11-dev \
  libvulkan-dev mesa-vulkan-drivers \
  libasound2-dev
```

### Versione di libav

Se il tuo sistema dispone di una versione differente rispetto a quella linkata, es:

```
error while loading shared libraries: libavutil.so.60: cannot open shared object file: No such file or directory
```

è possibile forzare una versione specifica di `libavutil` tramite la variabile `LD_LIBRARY_PATH`, es:

```
export LD_LIBRARY_PATH="/percorso/libreria/locale/:$LD_LIBRARY_PATH"
./vv-app
```

per utilizzare una versione differente locale.


## Build

Dalla root del workspace:

```sh
cargo build -p vv-app            # debug
cargo build -p vv-app --release  # release (ottimizzato, molto più lento da compilare)
```

Il primo build è lento (bindgen di `ffmpeg-next` + compilazione di `wgpu`);
i successivi sono incrementali.

### AppImage

```sh
scripts/build-appimage.sh
```

Produce `target/appimage/VibeVideo-<arch>.AppImage` con dentro FFmpeg
compilato da sorgente (librerie condivise, con libx264, zlib e NVENC) e
libx264: sulla macchina di destinazione non servono né FFmpeg né RPM Fusion.
glibc, ALSA, Vulkan e l'eventuale driver NVIDIA (NVENC richiede >= 550)
restano quelli di sistema.

Servono `git`, `nasm`, `make`, un compilatore C, `curl` e zlib (header),
oltre alle dipendenze non FFmpeg elencate sopra. FFmpeg viene compilato una
volta sola in `target/ffmpeg-shared`.

L'AppImage richiede una glibc almeno pari a quella della macchina di build:
per la massima compatibilità conviene compilarla in un container con una
distro vecchia, es. Debian bookworm (glibc 2.36):

```sh
podman run --rm -it -v "$PWD:/workspace:Z" -w /workspace docker.io/library/debian:bookworm bash
# dentro il container:
apt-get update && apt-get install -y curl ca-certificates git nasm make gcc g++ \
  pkg-config clang libclang-dev zlib1g-dev libwayland-dev libxkbcommon-dev \
  libx11-dev libvulkan-dev libasound2-dev file
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal && . ~/.cargo/env
scripts/build-appimage.sh
```

L'AppImage è GPL (include libx264) e non include FDK-AAC: l'export usa
l'encoder AAC nativo di FFmpeg.

## Eseguire

```sh
cargo run -p vv-app
```

Serve un backend grafico funzionante a runtime (Vulkan su Linux, tramite
`mesa-vulkan-drivers` o il driver GPU proprietario): senza, `wgpu` non trova
un adapter e la finestra non si apre.

## Test

```sh
cargo test -p vv-app -- --test-threads=1
```

`--test-threads=1` non è opzionale per ora: c'è un flake intermittente
(SIGSEGV, non ancora indagato) quando i test girano in parallelo — a thread
singolo sono stabili. Alcuni test invocano `ffmpeg` da riga di comando per
generare clip sintetiche in `/tmp`, quindi serve anche il binario `ffmpeg`
(non solo le librerie di sviluppo) nel `PATH`.

## Lint

```sh
cargo clippy --all-targets
cargo fmt --check
```

## Container di test headless

[`container/`](container/README.md) contiene un ambiente Podman per
compilare/eseguire/screenshottare vv-app senza una sessione grafica reale
(Xvfb + Vulkan software) — utile per verificare la UI in isolamento o da
un agente senza accesso al display dell'utente.

## Licenza

GPL-3.0-or-later, vedi [LICENSE](LICENSE).
