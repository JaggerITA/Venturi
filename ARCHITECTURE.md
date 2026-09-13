# vibevideo — architettura

Editor video "solo edit page" in stile DaVinci Resolve: taglio multi-traccia,
trasformazioni di base (solid color, text, crop, zoom, speed, audio gain),
ripple/normal delete. Niente node editor, niente color correction.

Target primario: Asahi Linux (Fedora Asahi Remix) su Apple Silicon, sorgenti
principalmente H.264 (x264) 1080p, in RAM/VRAM contenute.

## Decisioni chiave (e perché)

| Area | Scelta | Motivazione |
|---|---|---|
| Linguaggio | Rust | ecosistema media maturo, zero GC, sicurezza in editing veloce con AI |
| UI | egui + eframe | immediate-mode, puro Rust, condivide il device wgpu col compositor |
| GPU | wgpu su Vulkan (Honeykrisp) | conformante 1.3/1.4 su M1/M2, stack unico per UI e compositing |
| Decode video | FFmpeg software (ffmpeg-next / libavcodec) | il decoder HW V4L2/AVD è ancora instabile con multi-reference frame (praticamente ogni file x264 reale) |
| Encode/export | FFmpeg software (libx264 via ffmpeg-next) | nessun encoder HW affidabile su Asahi oggi |
| Time-stretch audio | filtro `rubberband` di libavfilter (ffmpeg di sistema è già compilato con `--enable-librubberband`) | pitch preservato, zero binding extra da scrivere |
| Persistenza progetto | RON, leggibile | debuggabile, diffabile con git |
| Undo/redo | command pattern (comandi invertibili) | history illeggera, illimitata, coerente con architettura a dati |
| Frame rate | per-Timeline (non per-Project): un Project contiene N Timeline, ognuna con il proprio fps target a cui le clip si conformano | come in Resolve: Project = contenitore, Timeline = sequenza con fps proprio |
| Ripple delete | globale su tutte le tracce (chiude il gap ovunque, mantiene sync A/V) | scelta esplicita per editing multi-traccia sincronizzato |
| Keyframe | supportati su tutti i parametri di trasformazione (crop/zoom/text/gain) | richiesto esplicitamente |
| Cache | frame cache LRU in RAM + proxy files generati in background | miglior compromesso perf/UX su long-GOP x264 |
| Risoluzione target | 1080p principale | dimensiona i budget di cache di default |
| Waveform | sì, in timeline | utile per tagliare su pause del parlato |

## Modello dati (data-oriented, niente grafo a nodi)

Storage ibrido: `MediaItem` e `Timeline` vivono in arene `slotmap`
(lookup O(1) per ID, pochi elementi, nessuna iterazione critica); le `Clip`
vivono in `Vec<Clip>` dentro ogni `Track`, ordinate per tempo, con un
`ClipId` a contatore semplice — l'iterazione sequenziale per il compositing
beneficia della località in memoria più di quanto serva l'indirection di
un'arena. Niente `Rc<RefCell<..>>` sparsi: un'unica struct `Project` è la
source of truth, posseduta dal thread UI; tutto il resto comunica via
canali/messaggi.

```rust
struct Project {
    media_pool: SlotMap<MediaId, MediaItem>,
    timelines: SlotMap<TimelineId, Timeline>,
}

struct MediaItem {
    path: PathBuf,
    probed: MediaMeta,   // durata, fps sorgente, risoluzione, canali audio
    content_hash: u64,   // per chiave cache/proxy
}

struct Timeline {
    name: String,
    fps: Rational,
    resolution: (u32, u32),
    tracks: Vec<Track>,  // ordine = ordine di compositing, bottom→top
}

struct Track {
    kind: TrackKind, // Video | Audio
    clips: Vec<Clip>, // sempre ordinate per start_time, non sovrapposte
    muted: bool,
}

struct Clip {
    id: ClipId,
    source: ClipSource,           // Media(MediaId) | SolidColor
    source_in: FrameIdx,
    source_out: FrameIdx,
    timeline_start: FrameIdx,     // in frame di Timeline (dopo conform fps)
    effects: EffectStack,         // fisso, niente grafo dinamico
}

struct EffectStack {
    transform: Keyframed<Transform>,   // crop rect + zoom + posizione
    speed: Keyframed<f32>,             // 1.0 = normale
    gain_db: Keyframed<f32>,           // solo se ha audio
    text: Vec<TextOverlay>,            // 0..n, ognuno keyframeable
    color: Option<Keyframed<Rgba>>,    // solo per SolidColor
}

struct Keyframed<T> { keyframes: Vec<(FrameIdx, T, Interpolation)> }
```

Le trasformazioni sono valutate a runtime (CPU, economico) in base al frame
corrente e passate come uniform alla GPU — **non vengono mai bake-ate nei
frame cachati**, così editare un parametro non invalida la cache dei frame
decodificati.

## Pipeline di decode + cache

- Un decoder ffmpeg "caldo" per ogni clip vicina al playhead.
- Seek: vai al keyframe H.264 più vicino ≤ target, decodifica sequenziale
  fino al frame richiesto, scartando gli intermedi (i long-GOP x264 rendono
  il seek casuale intrinsecamente costoso: da qui la cache).
- Frame cache: `LruCache<(MediaId, SourceFrameIdx), DecodedFrame>` — chiave
  sul *media* sorgente, non sulla clip: due clip che referenziano lo stesso
  file (anche con trim diversi, o duplicate su più track) condividono la
  stessa cache. Frame *raw* post color-space-detect, pre-effetti; pool di
  worker thread per il decode-ahead durante playback/scrub, budget di
  default ~1.5 GB RAM per 1080p (~500 frame NV12/YUV420).
- Proxy: generazione in background (ffmpeg, H.264 all-intra a 960px di
  larghezza) per ogni media importato, salvati in `.vibevideo/proxies/`
  accanto al progetto, chiave = `content_hash`. Toggle "usa proxy" in UI
  come in Resolve; l'export usa sempre i sorgenti originali.
- Waveform: pre-pass di peak extraction all'import (ffmpeg `-af
  aformat,astats` o lettura diretta dei sample), cachato su disco insieme ai
  metadata del media.

## Compositing GPU (wgpu, per ogni frame di output)

1. Per ogni track (bottom→top) al tempo corrente: risolvi la clip attiva,
   mappa tempo-timeline → frame-sorgente (considerando speed/time-remap),
   prendi il frame dalla cache (bloccante breve in scrub, async in
   playback).
2. Upload texture YUV (planare) in GPU, conversione YUV→RGB in shader
   (evita conversioni CPU).
3. Shader applica crop (sample rect) + zoom/posizione (trasforma la quad)
   con i parametri interpolati per il frame corrente.
4. Overlay testo: glyph atlas pre-rasterizzato via `cosmic-text` (shaping +
   layout) cachato per (contenuto+stile), disegnato come quad aggiuntive.
5. Clip "SolidColor": stesso shader, skip del texture sampling, fill diretto.
6. Blend over (alpha standard) accumulando track per track nel framebuffer
   di output, poi presentato nella texture del viewer egui (`egui-wgpu`).

## Pipeline audio

- Decode (ffmpeg) → resample al sample rate di progetto (`rubato` o
  `swresample`) → gain (moltiplicazione lineare da dB, keyframeable,
  interpolata a blocchi) → time-stretch se speed ≠ 1 (filtro `rubberband`
  via `ffmpeg-next::filter`, sia in preview sia in export) → mix delle
  track attive → output via `cpal`.
- Durante il playback l'audio è il clock master (prassi standard per
  percepire fluidità): il frame video mostrato insegue la posizione audio
  corrente.

## Undo/redo

`trait Command { fn apply(&mut self, p: &mut Project); fn undo(&self, p: &mut
Project); }`. Ogni comando cattura lo stato "prima" necessario per il suo
`undo` al momento dell'esecuzione. Stack `Vec<Box<dyn Command>>` per undo,
svuotato/il redo-stack pulito a ogni nuovo comando. Esempi: `InsertClip`,
`RippleDeleteAllTracks`, `LiftDelete` (normal delete, lascia il gap),
`TrimClip`, `SplitClip`, `SetEffectKeyframe`, `MoveClip`.

## Threading

- **Main/UI thread**: possiede `Project`, loop egui, dispatch dei comandi,
  presenta il framebuffer wgpu.
- **Decode pool**: worker dedicati (≈ metà dei core disponibili),
  ricevono richieste `(ClipId, FrameIdx)` da uno scheduler guidato dal
  playhead, scrivono nella cache condivisa.
- **Audio thread**: callback `cpal`, consuma un ring buffer lock-free
  riempito da un thread di mixing dedicato.
- **Background pool**: generazione proxy + waveform, priorità bassa.

## Struttura del workspace Cargo

```
vibevideo/
  crates/
    vv-core/     # modello dati, comandi, undo/redo, (de)serializzazione RON
    vv-media/    # decode ffmpeg, frame cache, proxy, waveform, probing
    vv-render/   # compositor wgpu, shader wgsl, text rasterization
    vv-audio/    # decode/mix/gain/time-stretch, output cpal
    vv-app/      # egui UI (timeline, viewer, toolbar), main.rs
  ARCHITECTURE.md
```

## Milestone incrementali

1. ✅ **Scaffold**: workspace Cargo, finestra egui vuota, apertura di un
   file video, decode del primo frame, disegno su una texture wgpu nel
   viewer.
2. ✅ **Playback lineare**: play/pause/seek su una singola clip, frame
   cache base (LRU per FrameIdx, decode-ahead su thread dedicato), audio
   sincronizzato via cpal (l'audio è il clock master; fallback a wall-clock
   se la clip non ha audio).
3. ✅ **Timeline minima**: multi-traccia, drag delle clip (con clamp contro
   i vicini), normal delete (lift), split, tutto con undo/redo. L'anteprima
   video resta per-media (non ancora "segue il playhead della timeline
   composita"): quella parte arriva naturalmente con il compositor
   (milestone 5), che deve comunque risolvere "quale clip è attiva a che
   tempo" per il rendering multi-track.
4. ✅ **Ripple delete** globale multi-traccia (Shift+Del / pulsante
   dedicato, accanto al normal delete Del/Backspace della milestone 3).
5. ✅ **Trasformazioni**: crop, zoom, gain, statici e a keyframe.
   Compositor GPU wgpu per crop/zoom, gain realtime nel callback audio
   (control-rate ~60Hz per l'automazione keyframeata, non ancora
   sample-accurate — sufficiente per l'anteprima, l'export potrà fare di
   meglio se servirà), pannello proprietà con toggle diamante per
   animare/aggiungere/rimuovere un keyframe al frame corrente, tutto con
   undo/redo. Interpolazione Hold/Linear/EaseInOut in `Keyframed::value_at`.
6. 🟡 **SolidColor + Text overlay** — **SolidColor fatto**: clip
   generatore (riempimento CPU, il crop/zoom di un colore piatto è un
   no-op quindi non serve il compositor GPU per questo), colore statico o
   keyframeato con color picker nel pannello proprietà, valutato sul frame
   locale alla clip derivato dal playhead della timeline (l'unico orologio
   sensato per un generatore, che non ha un Player). **Text overlay non
   ancora fatto**.

   **Fix post-milestone, round 1** (bug reali segnalati dall'utente):
   play/pause con Space; tasto "dividi" spostato da S a T; clip
   audio+video dello stesso import collegate di default (`Clip::linked`,
   menu contestuale per collegare/scollegare — drag di una clip collegata
   muove anche la gemella, vincolato dai limiti di *entrambe*); scrub del
   playhead della timeline che fa il seek del player della clip attiva.

   **Fix post-milestone, round 2** (playback timeline-aware, altri bug
   reali): il player non era più legato alla selezione ma al *playhead
   sulla track video* (`ensure_active_clip_matches_playhead` +
   `load_video_clip`), quindi Play/Spazio funziona senza selezionare
   nulla; `drive_playback` avanza automaticamente alla clip successiva
   sulla track video quando la riproduzione raggiunge la fine del *trim*
   di quella attiva (non della fine del file), invece di continuare oltre
   il taglio; il tasto T (`split_all_at_playhead`) taglia tutte le clip
   sotto al playhead su ogni track in un colpo solo (un `CompositeCommand`,
   un solo undo), senza bisogno di selezione. Limite noto: l'avanzamento
   automatico si ferma se la prossima clip è un generatore SolidColor
   (nessun player fa da orologio per un riempimento piatto — vedi
   `drive_playback`).

   **Fix post-milestone, round 3** (transizione tra clip non istantanea,
   selezione collegata mancante, scrub ignorato in playback): il salto tra
   clip che condividono lo stesso `MediaId` (es. due tagli dello stesso
   import) riapriva sempre il `Player` da zero — chiudeva il decoder,
   ne apriva un altro, ridecodificava tutto l'audio — causando un flash
   nero percepibile di decine di millisecondi. `load_video_clip` ora
   confronta il media della clip precedente e di quella nuova
   (`can_reuse_player_for`, funzione pura testata a parte) e se coincidono
   riusa lo stesso `Player` facendo solo un seek (e nemmeno quello se il
   frame sorgente target coincide già con quello corrente, dato che pure
   un seek sullo stesso player forza un flush/keyframe-seek in ffmpeg).
   `preview_media` inoltre non azzera più `frame_texture` prima di aprire
   il nuovo player: il viewer mostra l'ultimo frame valido finché non ne
   arriva uno nuovo, invece di mostrare un riquadro vuoto. La selezione di
   una clip video non evidenziava la traccia audio collegata pur
   applicando le operazioni ad entrambe (`selected_linked_clip_id` in
   `timeline_ui.rs`, usata per estendere l'evidenziazione della selezione
   alla gemella `linked`); delete e ripple-delete ora agiscono sempre
   anche sulla clip collegata (`linked_partner` + `CompositeCommand` /
   `RippleDeleteAllTracks::with_also_remove`, che calcola lo shift una
   sola volta per evitare il doppio spostamento che si otterrebbe
   chiamando ripple-delete due volte in sequenza). Infine, lo scrub della
   timeline durante la riproduzione veniva ignorato dal player attivo:
   `ensure_active_clip_matches_playhead` accetta ora un parametro
   `force_seek` che distingue uno spostamento del playhead causato
   dall'utente (scrub/click sulla timeline, rilevato confrontando il
   playhead prima/dopo `show_timeline()` nel frame corrente) da uno
   causato da `drive_playback` stesso, e forza il seek del player solo
   nel primo caso.
7. **Speed change** + time-stretch audio.
8. **Proxy workflow** + waveform in timeline.
9. **Export**: pipeline di encode ffmpeg che applica l'intero stack di
   effetti e produce il file finale.
10. **Persistenza progetto** (RON) + undo/redo completo su tutte le
    operazioni sopra.

## Setup ambiente richiesto

```
sudo dnf install ffmpeg-devel vulkan-loader-devel vulkan-headers clang
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
