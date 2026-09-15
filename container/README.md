# Container di test (headless)

Ambiente Podman per compilare/eseguire/screenshottare vv-app senza una
sessione grafica reale — Xvfb come X server virtuale, Vulkan software
(lavapipe, già in `mesa-vulkan-drivers`) per `wgpu` invece di richiedere
una GPU reale passata al container. Il repo non è dentro l'immagine: va
montato a runtime, così riflette sempre lo stato locale corrente senza
dover ricostruire l'immagine a ogni modifica.

## Setup (una volta)

```sh
podman build -t vibevideo-test -f container/Containerfile container/
```

## Uso rapido (comando singolo, container usa-e-getta)

```sh
container/run.sh cargo build -p vv-app
container/run.sh cargo test -p vv-app -- --test-threads=1
container/run.sh bash   # shell interattiva
```

## Uso per testare la UI (sessione persistente)

Un comando singolo (`run.sh`) non basta per "avvia l'app, interagisci,
screenshot, interagisci di nuovo" — serve un container che resti vivo
tra un comando e l'altro:

```sh
container/session.sh start                      # avvia Xvfb + il container
container/session.sh exec cargo build -p vv-app

# Avvia l'app in background dentro la sessione (il binario sta in
# /cargo-target/debug/, non nel $PATH — vedi CARGO_TARGET_DIR nel
# Containerfile):
container/session.sh exec bash -c \
    'nohup /cargo-target/debug/vv-app >/tmp/vv-app.log 2>&1 & disown'

container/session.sh shot avvio                  # -> container/shots/avvio.png
container/session.sh exec xdotool mousemove 162 11 click 1   # apre un menu
container/session.sh shot menu-aperto

container/session.sh exec pkill -f vv-app
container/session.sh stop                        # ferma e rimuove il container
```

`xdotool` prende coordinate assolute sullo schermo virtuale
(1280x800 di default, `XVFB_RESOLUTION` nel Containerfile) — usa uno
screenshot precedente per leggere a occhio le coordinate del prossimo
click. Comandi utili: `mousemove X Y`, `click 1`/`click 3` (tasto
sinistro/destro), `key <nome>` (es. `key space`), `keydown`/`keyup`
per un drag (`mousedown 1` ... `mousemove` ... `mouseup 1`).

## Cache tra le run

`run.sh`/`session.sh` montano due volumi Podman nominati
(`vibevideo-cargo-registry`, `vibevideo-target`) così le dipendenze
scaricate e gli artefatti di build restano tra un container e l'altro
— solo il primo build è lento (bindgen di `ffmpeg-next` + `wgpu`,
~2 minuti), i successivi sono incrementali. Per ripartire da zero:
`podman volume rm vibevideo-cargo-registry vibevideo-target`.

## Limiti noti

- Nessun audio reale (`/etc/asound.conf` punta a un device ALSA nullo):
  basta perché `cpal` non fallisca l'apertura dello stream, non per
  sentire nulla — irrilevante per test visivi sulla timeline/UI.
- Rendering software (lavapipe): funzionalmente corretto ma più lento
  di una GPU reale — va bene per uno screenshot o un giro di
  interazione, non per misurare framerate/performance percepite.
- Nessun window manager: le finestre non hanno decorazioni e non c'è
  focus-follow-mouse — `xdotool` interagisce comunque via coordinate
  assolute, ma azioni che assumono un WM (es. alt-tab) non si
  applicano.
