# Piano: clip con fps diverso da quello della timeline

Bug: una clip il cui media ha un fps diverso da quello della timeline va
fuori sync, perché il video la mappa 1:1 sui frame di timeline mentre
l'audio suona a tempo reale. Misurato su una timeline a 60 fps con una
seconda clip a 59,94: deriva dello 0,1%, l'audio è ~0,84 s in ritardo dopo
14 minuti di quella clip. Vale anche per l'export. Bug preesistente, non
introdotto dal lavoro su anteprima/mixer.

## Dove sta oggi

- `Clip::timeline_len()` = `source_out - source_in` (`vv-core/src/model.rs`):
  mescola frame sorgente e frame di timeline.
- `Clip::source_frame_at(t)` = `source_in + (t - timeline_start)`: 1:1.
- `MixSnapshot::from_timeline` (`vv-audio/src/mixer.rs`) converte invece in
  secondi con l'fps del *media* (`clip_fps`): audio a velocità reale.
- `VibeVideoApp::insert_media_clip` (`vv-app/src/main.rs`) mette
  `source_out = meta.duration_frames`, cioè frame del media.

## Scelta: mappatura a tempo, con `rate` sulla clip

Una clip dura in timeline quanto dura davvero; il video ripete o salta un
frame quando serve (a 59,94 su 60: un frame duplicato ogni ~17 s).

```rust
// vv-core/src/model.rs
pub struct Clip {
    // ...
    /// Frame di timeline per frame sorgente: `fps timeline / fps media`.
    /// 1/1 per SolidColor e per i media allo stesso fps della timeline.
    #[serde(default = "Rational::one")]
    pub rate: Rational,
}

impl Clip {
    pub fn timeline_len(&self) -> FrameIdx      // round((source_out - source_in) * rate)
    pub fn source_frame_at(&self, t: FrameIdx) -> FrameIdx // source_in + round((t - timeline_start) / rate)
    pub fn source_len(&self) -> FrameIdx        // source_out - source_in, dove serve il conto in frame sorgente
}
```

Perché un rapporto e non un `timeline_len` memorizzato: `source_in/out`
restano l'unica fonte di verità, così un trim non può far divergere i due
valori. Perché non passare gli fps come parametri: `timeline_len()` e
`timeline_end()` sono chiamati in 66 punti, quasi tutti senza la `Timeline`
sottomano.

Serve un po' di aritmetica su `Rational` in `model.rs` (moltiplicare e
dividere un `FrameIdx`, con arrotondamento), più `Rational::one()`.

## Passi

1. **`vv-core`**: campo `rate`, aritmetica su `Rational`, `timeline_len` e
   `source_frame_at` che lo usano, `source_len()` per i punti che oggi
   fanno `source_out - source_in` a mano. Test: clip a 59,94 su timeline a
   60 (durata in timeline, mappatura agli estremi, nessuna deriva a 15
   minuti), clip a 25 su 30, `rate` 1/1 identica a oggi.
2. **Comandi** (`vv-core/src/command.rs`): `TrimClip` riceve un nuovo
   `source_in`/`source_out` in frame *sorgente* (già così), ma chi lo
   costruisce parte da pixel/frame di timeline: convertire là, non nel
   comando. `SplitClip` calcola `split_source = source_in + offset`
   (riga ~828): deve passare per `source_frame_at`. Test: split di una
   clip a fps diverso, le due metà coprono esattamente l'originale, senza
   buchi né sovrapposizioni; undo/redo invariati.
3. **Inserimento** (`vv-app/src/main.rs::insert_media_clip`): calcola
   `rate = timeline.fps / meta.fps` e lo mette sia sulla clip video sia su
   quelle audio. Attenzione anche a `MediaDrag` (in/out dell'anteprima,
   frame del media) e al ghost del drag&drop in `timeline_ui`, che usa
   `drag.len()` come larghezza: va convertito in frame di timeline.
4. **Trim in timeline** (`vv-app/src/timeline_ui.rs`): i bordi si
   trascinano in frame di timeline e vanno convertiti in frame sorgente
   col `rate` della clip; stessa cosa per i limiti di trim
   (`media_duration_frames`, riga ~2555).
5. **Audio** (`vv-audio/src/mixer.rs::from_timeline`): con la mappatura a
   tempo, `timeline_len / fps_timeline` è già la durata reale. Sostituire
   i conti basati su `clip_fps` con quelli sull'fps della timeline, sia
   per `start`/`len` sia per la valutazione del gain keyframeato. Test:
   una clip a 59,94 su timeline a 60 dura in campioni quanto il suo
   audio, e a 14 minuti il mix non è spostato.
6. **Export e buffer video**: `vv-app/src/export.rs` e
   `render_ahead.rs`/`frame_provider.rs` passano già da
   `source_frame_at`/`timeline_len`; verificare che non restino conti a
   mano (`render_ahead.rs` righe ~1666-1702). Test di export end-to-end
   con due clip a fps diverso in coda: durata del file e sync a fine
   timeline.
7. **Progetti salvati**: `rate` ha default 1/1, quindi i progetti vecchi
   si caricano. In `VibeVideoApp::load_project_from` ricalcolare `rate`
   per ogni clip Media dai metadati del media e dall'fps della timeline,
   così i progetti salvati prima della correzione si sistemano da soli.
   Test: round trip salva/carica, e un progetto con `rate` assente che
   viene corretto al caricamento.
8. **Documentazione**: aggiornare `ARCHITECTURE.md` (modello dati e nota
   sul fatto che una clip a fps diverso viene conformata) e togliere da
   "Non ancora" ciò che non vale più.

## Verifica finale

Riprodurre il caso reale: timeline creata da un media a 60 fps, seconda
clip a 59,94 messa in coda, riproduzione intorno al minuto 29. L'audio
deve restare allineato al video. Stessa prova sull'export dell'intervallo
in/out.

## Da non dimenticare

- L'anteprima dal media pool non passa da `Clip`: usa l'fps del media per
  testina e audio, quindi resta com'è.
- I marker in/out dell'anteprima sono in frame del media: `MediaDrag` li
  trasporta così, e la conversione avviene all'inserimento.
- Non toccare `EffectStack::speed` (time remap): è un'altra cosa, non
  ancora implementata.
