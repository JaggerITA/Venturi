# vibevideo — architettura

Editor video "solo edit page" in stile DaVinci Resolve: taglio multi-traccia,
trasformazioni di base (solid color, text, crop, zoom, speed, audio gain),
ripple/normal delete. Niente node editor, niente color correction.

Target primario: Asahi Linux (Fedora Asahi Remix) su Apple Silicon, sorgenti
principalmente H.264 (x264) 1080p, in RAM/VRAM contenute.

Questo documento descrive lo stato attuale. La storia delle scelte è nella
git history; le regole del refactor della pipeline (invarianti, vincoli)
sono in [REFACTOR_PIPELINE.md](REFACTOR_PIPELINE.md).

## Decisioni chiave (e perché)

| Area | Scelta | Motivazione |
|---|---|---|
| Linguaggio | Rust | ecosistema media maturo, zero GC, sicurezza in editing veloce con AI |
| UI | egui + eframe | immediate-mode, puro Rust, condivide il device wgpu col compositor |
| GPU | wgpu su Vulkan (Honeykrisp) | conformante 1.3/1.4 su M1/M2, stack unico per UI e compositing |
| Decode video | FFmpeg software (ffmpeg-next / libavcodec) | il decoder HW V4L2/AVD è ancora instabile con multi-reference frame (praticamente ogni file x264 reale) |
| Encode/export | FFmpeg via ffmpeg-next: NVENC se si apre davvero, altrimenti libx264 | nessun encoder HW affidabile su Asahi oggi |
| Time-stretch audio | filtro `rubberband` di libavfilter (ffmpeg di sistema è già compilato con `--enable-librubberband`) | pitch preservato, zero binding extra da scrivere |
| Persistenza progetto | RON, leggibile | debuggabile, diffabile con git |
| Undo/redo | command pattern (comandi invertibili) | history leggera, illimitata, coerente con architettura a dati |
| Frame rate | per-Timeline (non per-Project): un Project contiene N Timeline, ognuna con il proprio fps | come in Resolve: Project = contenitore, Timeline = sequenza con fps proprio |
| Ripple delete | globale su tutte le tracce (chiude il gap ovunque, mantiene sync A/V) | scelta esplicita per editing multi-traccia sincronizzato |
| Keyframe | su tutti i parametri di trasformazione (crop/zoom/gain/colore) | richiesto esplicitamente |
| Cache | cache frame in RAM a budget globale, sfratto per distanza dalla testina + proxy tutto-intra generati in background | miglior compromesso perf/UX su long-GOP x264 |
| Risoluzione target | 1080p principale | dimensiona i budget di cache di default |
| Waveform | sì, in timeline | utile per tagliare su pause del parlato |

## Modello dati (data-oriented, niente grafo a nodi)

`vv-core/src/model.rs`. `MediaItem` e `Timeline` vivono in arene `slotmap`
(lookup O(1) per ID); le `Clip` in `Vec<Clip>` dentro ogni `Track`, ordinate
per tempo, con `ClipId` a contatore. Un'unica struct `Project` è la source
of truth, posseduta dal thread UI; i worker ricevono copie o snapshot.

```rust
struct Project {
    media_pool: SlotMap<MediaId, MediaItem>,
    timelines: SlotMap<TimelineId, Timeline>,
    // + contatori per ClipId e LinkGroupId
}

struct MediaItem {
    path: PathBuf,
    meta: MediaMeta,     // durata, fps, risoluzione, audio (rate/canali)
    content_hash: u64,   // chiave di proxy e waveform
}

struct Timeline {
    name: String,
    fps: Rational,
    resolution: (u32, u32),
    tracks: Vec<Track>,  // ordine = ordine di compositing, bottom→top
}

struct Track {
    kind: TrackKind,     // Video | Audio
    clips: Vec<Clip>,    // ordinate per timeline_start, non sovrapposte
    muted: bool,
}

struct Clip {
    id: ClipId,
    source: ClipSource,               // Media(MediaId) | SolidColor
    source_offset: FrameIdx,          // inizio nel media, in frame di Timeline
    timeline_start: FrameIdx,         // in frame di Timeline
    timeline_len: FrameIdx,           // in frame di Timeline
    effects: EffectStack,
    linked_group: Option<LinkGroupId>, // video + audio dello stesso import
    audio_stream_index: usize,         // quale stream audio del media
    rate: Rational,                    // frame di Timeline per frame sorgente
}

struct EffectStack {
    transform: TransformTracks,  // un Keyframed<f32> per parametro + flip
    speed: Keyframed<f32>,            // nel modello, non ancora applicato
    gain_db: Keyframed<f32>,
    text: Vec<TextOverlay>,           // nel modello, non ancora applicato
    color: Option<Keyframed<Rgba>>,   // solo per SolidColor
}

struct Keyframed<T> { keyframes: Vec<(FrameIdx, T, Interpolation)>, default: T }
```

- Import di un media: una clip video più una clip audio per ogni stream
  audio, tutte nello stesso `linked_group`. Selezione, drag e delete
  trattano il gruppo come un'unità.
- Le trasformazioni si valutano a runtime (`Keyframed::value_at`, CPU) e
  vanno allo shader come uniform: **mai bake-ate nei frame in cache**, così
  editare un parametro non invalida il buffer.
- Una clip il cui media ha un fps diverso da quello della Timeline viene
  **conformata**: `Clip::rate` (= fps Timeline / fps media, calcolato
  all'inserimento e ricalcolato al caricamento di un progetto) è l'unico
  posto dove i due spazi frame si incontrano. `source_frame_at()`/
  `timeline_frame_at()` lo applicano, quindi una clip dura in Timeline il
  suo tempo reale e il video ripete o salta un frame quando serve (a 59,94
  su 60: uno duplicato ogni ~17 s) invece di andare fuori sync con
  l'audio, che suona sempre a velocità reale.
- L'intervallo di una clip è `source_offset` + `timeline_len`, entrambi in
  frame di Timeline (come `source_range` di OTIO); `source_in()`/
  `source_out()` ne sono derivati. Split e trim cadono esattamente sulla
  posizione scelta anche a metà di un frame sorgente, senza perdere la
  fase del contenuto (vedi `DURATA_CLIP_ESPLICITA.md`).

## Pipeline di decode + cache

- **Decode** (`vv-media/src/decode.rs`): `Decoder` con `next_frame` e
  `seek_to_time` (keyframe ≤ target, poi decode sequenziale). Ogni formato
  pixel viene normalizzato a YUV420P 8-bit (`FrameYuv420`); la conversione
  a RGB è nello shader.
- **Buffer della timeline** (`vv-app/src/render_ahead.rs`, `RenderAhead`):
  un thread cammina la timeline in avanti dal playhead (`lookahead_secs`,
  default 3s; dietro tiene `behind_secs`, default 2s, col budget che
  avanza), attraversando tagli, vuoti e track senza casi speciali, e riempie una `SharedFrameCache`
  (`vv-media/src/cache.rs`) a chiave `(MediaId, frame sorgente)` con budget
  globale in byte (default 1.2 GB, configurabile). Lo sfratto è un solo
  pass (`reconcile`): fuori finestra via, poi i frame più lontani dalla
  testina. La soglia oltre cui conviene un seek reale invece di decodificare
  in avanti si adatta al GOP osservato. Lookahead e behind sono
  configurabili dal menu Playback > Proxy; a velocità > 1x il lookahead scala.
- **Anteprima dal media pool** (`browsing_media`): `vv_media::DecodeAhead`
  su un solo file con una `FrameCache` propria, seek dalla barra sotto al
  viewer. L'audio passa dallo stesso `TimelineAudio` della timeline
  (`sync_media`: uno snapshot con tutti gli stream del media), il cui clock
  fa da testina.
- **Proxy** (`vv-media/src/proxy.rs`, thread `vv-app::proxy_worker`): per
  ogni media importato, H.264 tutto-intra a 960px di larghezza in
  `$XDG_CACHE_HOME/vibevideo/proxies/`, chiave `content_hash` (fingerprint
  veloce: path canonico + dimensione + mtime). Toggle nel menu Playback > Proxy,
  attivo di default; l'export usa sempre i sorgenti originali.
- **Waveform** (`vv-media/src/waveform.rs`, thread
  `vv-app::waveform_worker`): picchi per `(content_hash, stream)` calcolati
  in streaming e salvati su disco; la timeline li carica in memoria e li
  disegna sulle clip audio.

## Compositing GPU (wgpu, per ogni frame di output)

1. I layer di un frame sono le clip attive su ciascuna track video, dal
   basso verso l'alto (`Timeline::active_video_clips_at`); il frame sorgente
   di ognuna viene da `Clip::source_frame_at`, procurato tramite `FrameProvider`
   (`vv-app/src/frame_provider.rs`): dalla cache di `RenderAhead` in
   anteprima, in streaming nell'export.
2. Upload dei piani YUV, conversione a RGB nello shader
   (`vv-render/src/shaders/transform.wgsl`, matrice BT.601/709/2020 e range
   dal sorgente).
3. Lo shader inscrive il sorgente nel frame di output mantenendone
   l'aspect ratio — bande (letterbox/pillarbox) invece di deformare, es.
   una clip 9:16 in una timeline 16:9 — e ci applica il `Transform`
   ragionando in coordinate di *output*, con l'asse Y verso l'alto come in
   un NLE (lo shader lo gira, le uv puntano in basso) e i parametri in
   pixel — di timeline per posizione e anchor, del media (risoluzione
   nativa, non del proxy) per crop e sfumatura, convertiti in frazioni
   nell'uniform (`OutputFrame`, `Layer::Video::source_size`): `zoom` (per asse) e
   `rotation` agiscono attorno all'`anchor`, `position` sposta la clip nel
   frame, `flip` la specchia, `crop` taglia ciascun lato di una frazione
   (0 = intatto) — con `crop_softness` che sfuma il bordo via alpha, verso
   l'interno se negativa e verso l'esterno se positiva —
   senza ricentrare né ridimensionare il resto.
4. Un pass per layer sulla stessa texture, in alpha-over
   (`Compositor::render_layers`): le bande del layer sopra escono con alpha
   0 e lasciano vedere quello sotto. Una clip SolidColor è il clear del
   pass, non un draw.
5. Anteprima: compone alla risoluzione del frame decodificato allargata
   all'aspect della timeline (`vv_render::fit_output_size`), così le bande
   si vedono già in editing senza upscalare il contenuto;
   `Compositor::render_layers_to_texture` resta sulla GPU e la
   texture è registrata in `egui-wgpu`, senza readback. Export:
   `Compositor::render_layers_i420` compone alla risoluzione della timeline,
   converte in I420 BT.709 su GPU (compute shader) e fa il readback dei
   piani per l'encoder. Decode, composizione ed encode girano su tre
   thread in pipeline.
6. Senza clip video (vuoto su tutte le track): frame nero — in anteprima
   generato su CPU (`vv-render/src/generator.rs`), che resta anche per il
   caso "solo clip SolidColor".

Non c'è ancora opacità né blend-mode per-clip: un layer opaco che copre
tutto il frame occlude quelli sotto, l'alpha in gioco è solo quella delle
bande di letterbox.
Text overlay (`vv-render/src/text.rs`) non implementato.

## Pipeline audio

- Decode (ffmpeg, `vv_media::decode_audio_track`) → resample lineare a
  48 kHz e conversione ai canali del mix (`vv_audio::mixer::prepare_mix_buffer`)
  → mix delle track audio non muted con gain keyframeato valutato a blocchi
  da 800 campioni (`mix_range`) → output `cpal`.
- Anteprima ed export usano la stessa `mix_range`: l'export mixa a 2 canali,
  l'anteprima ai canali nativi del device (chiederne altri fa inserire a
  PipeWire un remix che aggiunge latenza).
- **Anteprima** (`vv-app/src/timeline_audio.rs`, `TimelineAudio`): un solo
  stream `cpal` aperto all'avvio e mai riaperto (`vv_audio::Mixer`). Il
  thread UI costruisce uno snapshot immutabile delle clip (`MixSnapshot`,
  con `Arc` ai buffer già convertiti) a ogni cambio di `History::generation`
  o all'arrivo di un buffer, e lo pubblica al callback con un `try_lock`;
  gli snapshot vecchi si liberano sul thread UI. I buffer si decodificano in
  background per `(path, audio_stream_index)` (`mix_buffers.rs`) e
  pubblicati parziali mentre crescono: suona subito l'inizio della traccia,
  silenzio solo oltre la parte già decodificata.
- **Clock**: la posizione del mixer, in campioni di timeline, è il playhead
  (`drive_playback`); il video la insegue. Un vuoto è solo silenzio, il
  clock avanza lo stesso. Senza device audio il clock è a parete.
- **Fast forward** (tasto "a", 2x/4x/8x): finestre da 8s del mix
  renderizzate e stretchate con `rubberband` in background, accodate al
  mixer senza riaprire lo stream (`StretchedWindow`); la velocità si applica
  quando la prima finestra è pronta.
- **Scrub**: frammento di 80ms dal mix alla nuova posizione (opzione nel
  menu Timeline).

## Undo/redo

`trait Command { fn apply(&mut self, p: &mut Project); fn undo(&self, p: &mut
Project); }` in `vv-core/src/command.rs`. Ogni comando cattura lo stato
"prima" al momento dell'esecuzione; `History` tiene gli stack undo/redo e una
`generation` che cambia a ogni modifica (i worker la confrontano per sapere
quando riallinearsi). Più comandi in un solo passo: `CompositeCommand`.
Comandi: `InsertClip`, `LiftDelete`, `RippleDeleteAllTracks`,
`RippleDeleteGap`, `MoveClip`, `MoveClips`, `TrimClip`, `SplitClip`,
`LinkClips`, `UnlinkClip`, `AddTrack`, `RemoveTrack`, `SetClipTransform`,
`SetClipGain`, `SetClipColor`, `UpsertKeyframe`, `RemoveKeyframe`.

## Threading

- **UI**: possiede `Project` e `History`, loop egui, dispatch dei comandi,
  compositing dell'anteprima.
- **Audio**: callback `cpal` del `Mixer`, mixa direttamente dallo snapshot
  corrente, senza allocazioni né lock bloccanti.
- **`RenderAhead`**: un thread di decode per il buffer della timeline (un
  pool multi-worker non è ancora servito).
- **`mix_buffers`**: decode e resample dei buffer audio del mixer.
- **Stretch**: un thread per finestra di fast forward, al più una richiesta
  in volo.
- **`proxy_worker`**, **`waveform_worker`**: code seriali, un thread
  ciascuno.
- **Export**: thread dedicato su un clone di `Project`, con progresso e
  annullamento condivisi.

## Struttura del workspace Cargo

```
vibevideo/
  crates/
    vv-core/     # modello dati, comandi, undo/redo, persistenza RON
    vv-media/    # probe, decode, cache frame, proxy, waveform, encode
    vv-render/   # compositor wgpu, shader wgsl, generatori
    vv-audio/    # mixer, resample, time-stretch, output cpal
    vv-app/      # egui UI (timeline, viewer, pannelli), playback, export
```

## Stato delle funzionalità

Fatto:
- Import (anche multi-stream audio), media pool con anteprima, drag sulla
  timeline (più elementi selezionati insieme vengono accodati nell'ordine
  del pannello), multi-selezione (click, ctrl, shift, rettangolo) e
  cancellazione (Del/Backspace) degli elementi: le clip che usavano un
  media cancellato restano in timeline in rosso e il viewer mostra "Media
  offline" (`vv_core::RemoveMedia`).
- Timeline multi-traccia: aggiunta track, drag, trim, split al playhead (T),
  normal delete (Del/Backspace), ripple delete (tasto "<"), copia/incolla,
  collega/scollega, multi-selezione (click, ctrl, shift, rettangolo),
  "selection follows playhead", calamita, zoom (Ctrl+/Ctrl-).
- Barra di riproduzione sotto al viewer (`vv-app/src/transport.rs`):
  testina, marker in/out (tasti I/O), play/pausa. Sull'anteprima del media
  pool in/out delimitano la porzione trascinata dal viewer sulla timeline;
  sulla timeline delimitano l'export (non salvati nel progetto).
- Playback: Spazio play/pausa, "a" fast forward, frecce frame per frame
  (tenute premute scorrono a 0.5x), audio di tutte le track, scrub audio,
  audiometer.
- Effetti: zoom X/Y (con link), posizione, rotazione, anchor point, flip,
  crop dei quattro lati con sfumatura (verso l'interno o l'esterno), tutto
  in pixel, gain, colore SolidColor; statici o a
  keyframe (Hold/Linear/EaseInOut) dal pannello proprietà, diviso nelle
  schede Video (sezioni Transform e Cropping, reset per parametro e per
  sezione) e Audio (gain). Ogni parametro del transform ha i propri
  keyframe (`TransformTracks`): il diamante della riga è rosso quando la
  testina è su un keyframe, e altrimenti porta le frecce per saltare al
  keyframe più vicino in quella direzione. I valori mostrati sono quelli della prima clip
  selezionata su quel tipo di track; ogni modifica va a tutte le altre come
  un solo comando (`CompositeCommand`, quindi un solo undo).
- Clip con fps diverso da quello della timeline conformate
  all'inserimento (`Clip::rate`), in anteprima e in export.
- Proxy e waveform in background.
- Media solo audio (wav, mp3, flac…): fps nominale `AUDIO_ONLY_FPS`, niente
  proxy né miniatura, in timeline solo clip audio.
- Export H.264 + AAC in MP4 (Ctrl+Shift+E) da una finestra di impostazioni:
  destinazione, intervallo in/out o tutta la timeline, encoder video
  (x264/NVENC) con preset e qualità, risoluzione ridotta, encoder audio
  (AAC nativo/FDK) con preset e bitrate. Default: NVENC e FDK se
  disponibili, altrimenti x264 `superfast` CRF 20 e AAC nativo. Le ultime
  impostazioni restano per la sessione.
- Progetto su file `.vvproj` in RON (Ctrl+O, Ctrl+S, Ctrl+Shift+S).
- OpenTimelineIO (`vv-core/src/otio/`, File → Esporta/Importa OTIO).
  Export: `source_range` all'fps della timeline, i buchi come `Gap`,
  effetti, gruppi collegati e stream audio in `metadata.vibevideo`.
  Import: apre il file come progetto nuovo quantizzando i tempi al frame
  della timeline; un file nostro torna identico, da altri editor si
  ricollegano video e audio dello stesso tratto, e quel che non si sa
  rappresentare (transizioni, effetti, riferimenti non a file) viene
  segnalato.

Non ancora:
- Speed change per-clip (`EffectStack::speed`) e time-remap.
- Text overlay.
- Opacità/blend per-clip.

## Setup ambiente

Dipendenze, build e test: vedi [README.md](README.md).
