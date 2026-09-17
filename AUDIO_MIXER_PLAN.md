# AUDIO_MIXER_PLAN — mixer audio continuo per l'anteprima

Piano di lavoro per sostituire l'audio per-clip dell'anteprima con un mixer
delle track audio della timeline. È il punto **B5** di
`REFACTOR_PIPELINE.md`. Nessun codice qui: è la spec da implementare a passi.

## 1. Problema

- `crates/vv-app/src/player.rs::Player` apre un `vv_audio::AudioPlayer` sul
  media della **clip video attiva**, sempre stream audio 0
  (`VibeVideoApp::open_audio_player`, `main.rs`). Le track audio non vengono
  mai lette.
- Conseguenza (bug segnalato): spostare, scollegare, eliminare o cambiare
  stream a una clip audio non cambia nulla in anteprima. Clip audio senza
  video sopra non suonano. L'export invece mixa correttamente
  (`export.rs::mix_audio_track`), quindi anteprima ed export divergono.
- `vv-audio/src/mixer.rs` è solo un TODO.
- La posizione dell'`AudioPlayer` è anche il **clock master** del playback:
  `drive_playback` deriva il playhead da `player.current_source_frame()`. Nei
  vuoti (niente clip video) serve un clock a parte, `gap_wall_clock`.

## 2. Architettura target

### Stream unico
- Un solo stream cpal aperto per tutta la vita dell'app (o della timeline),
  mai riaperto: riaprirlo blocca la UI per centinaia di ms (vedi commit
  `c077bdb`, `AudioPlayer::replace_samples`).
- Formato di mix fisso: 48 kHz, canali nativi del device (downmix/upmix come
  fa già `AudioPlayer::new`). Riusare le costanti/logica di `export.rs`
  (`PROJECT_SAMPLE_RATE`, `resample_and_remix`, `apply_gain`) spostandole in
  `vv-audio` invece di duplicarle.

### Clock
- Il clock del playback è la posizione del mixer in **frame audio di
  timeline** (campioni consumati dal callback, atomic), convertita in
  `FrameIdx` di timeline con l'fps della timeline.
- Un vuoto in timeline è solo silenzio: il clock avanza lo stesso.
  `gap_wall_clock` e tutta la logica di aggancio clip→clip in
  `drive_playback`/`advance_playback_past` diventano inutili.
- Il video resta com'è: `render_ahead` + `current_video_frame` inseguono il
  playhead.

### Snapshot della timeline per il thread audio
- Il callback realtime non può prendere lock contesi né allocare. Il thread
  UI costruisce uno **snapshot immutabile** delle clip audio rilevanti:
  per ogni clip su track audio non `muted` → `timeline_start`, durata,
  offset sorgente in campioni, `Arc` del buffer decodificato già a 48 kHz e
  canali del mix, gain (`Keyframed`, valutato a blocchi come `apply_gain`).
- Pubblicazione snapshot: `Arc` scambiato atomicamente (es. crate
  `arc-swap`, o `Mutex` preso con `try_lock` dal callback tenendo il vecchio
  se occupato). Ricostruirlo quando cambia il progetto (history version /
  dopo ogni `do_command`/undo/redo) e al cambio timeline.
- Cache dei buffer decodificati: chiave `(path, audio_stream_index)`, non più
  solo `path` (`audio_cache` in `main.rs`). Decodifica + resample in
  background: una clip il cui buffer non è pronto suona silenzio, non blocca.

### API (bozza) in `vv-audio`
- `Mixer::new() -> Result<Mixer>` apre lo stream.
- `set_snapshot(Arc<MixSnapshot>)`, `play()`, `pause()`,
  `seek(timeline_sample)`, `position() -> timeline_sample`,
  `set_speed(...)` (passo 4), `peak_linear_stereo()` (audiometer, oggi su
  `Player`).

## 3. Passi (uno alla volta, build + test dopo ognuno)

**Stato.** Passi 1 e 2 fatti. Il passo 2 ha anticipato:
- scrub audio (passo 3) già sul mixer (`TimelineAudio::play_scrub_snippet`);
- rimozione di `player.rs` e `audio_cache`: l'anteprima dal media pool non
  riproduceva mai (Spazio esce dal browsing), mostrava solo il primo frame, e
  così resta;
- fast forward temporaneo: a 2x/4x/8x il clock è a parete e l'audio tace
  finché il passo 4 non stretcha il mix.

1. **Mixer a 1x, in parallelo al vecchio Player.** Implementare `Mixer` +
   snapshot + cache buffer per `(path, stream)`. Test unitari puri sulla
   funzione di mix (dato snapshot e range di campioni → buffer atteso):
   clip sfasata, due clip sovrapposte sommate, track muted, gain, stream
   diverso, vuoto = silenzio. Il callback cpal chiama solo quella funzione.
2. **Agganciare il playback.** Play/pausa/seek passano dal mixer;
   `drive_playback` legge il playhead dal clock del mixer. Rimuovere
   `gap_wall_clock`, `active_clip` come sorgente audio, `open_audio_player`
   per la timeline. Audiometer dal mixer. Mantenere verdi (adattandoli solo
   se testano dettagli interni rimossi, non comportamenti) i test su
   playback, vuoti, selection follows playhead, frecce.
3. **Scrub audio** (`scrub_audio`, commit `fbfe05d`): frammento di ~80ms dal
   mix alla nuova posizione, stessa semantica attuale (non entra in
   riproduzione, ripristina la posizione).
4. **Fast forward** (tasto "a", `SpeedTier` 2x/4x/8x). Oggi: stretch a
   finestre di 8s del buffer di *un* media (`request_speed_window`,
   `poll_speed_stretch_result`, `SpeedWindowState`,
   `Player::begin_speed_window`). Target: stretch a finestre **del mix**
   (renderizzare il mix del range in background, stretcharlo con
   `vv_audio::stretch_samples`, suonarlo), stesse regole già fixate: seek
   fuori finestra riparte da lì senza riaprire lo stream e senza bloccare
   la UI; risultato superato scartato.
5. **Anteprima dal media pool** (`browsing_media`): oggi usa `Player` su un
   media singolo. Scegliere se tenerla su `Player` (più semplice) o farla
   passare dal mixer con uno snapshot di una sola "clip virtuale". Decidere
   con l'utente.
6. **Pulizia.** Rimuovere il codice morto di `player.rs` non più usato,
   aggiornare ARCHITECTURE.md (§ Pipeline audio) e B5 in
   REFACTOR_PIPELINE.md.

## 4. Vincoli

- Mai bloccare il thread UI (niente apertura stream, decode o stretch
  sincroni nel frame).
- Callback audio: niente allocazioni, niente lock bloccanti.
- Anteprima ed export devono produrre lo stesso mix (stesse funzioni di
  gain/resample).
- Regole di `CLAUDE.md` sui commenti (brevi, solo il perché).

## 5. Punti di partenza nel codice

- `crates/vv-app/src/player.rs` — Player attuale, scrub snippet, finestre
  di velocità.
- `crates/vv-app/src/main.rs` — `open_audio_player`, `load_active_clip_audio`,
  `ensure_active_clip_matches_playhead`, `drive_playback`,
  `advance_playback_past`, `toggle_playback`, `handle_fast_playback_key`,
  `request_playback_speed`, `request_speed_window`,
  `poll_speed_stretch_result`, `poll_speed_window_extension`,
  `seek_preview_player_to_frame`, `step_playhead_with_arrows`,
  `play_scrub_audio`, `raw_input_hook` (non c'entra), `audio_cache`.
- `crates/vv-app/src/export.rs` — `mix_audio_track`, `apply_gain`,
  `resample_and_remix`.
- `crates/vv-audio/src/output.rs` — `AudioPlayer` (downmix ai canali del
  device, `replace_samples`, `extend_samples`).
- `crates/vv-audio/src/stretch.rs` — `stretch_samples`.
- `crates/vv-media/src/audio.rs` — `decode_audio_track` (gestisce già layout
  canali non specificato).
