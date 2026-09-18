# Durata di timeline esplicita nella `Clip` — valutazione e piano

Documento di discussione. Nasce dal bug "lo split cade un frame
prima/dopo la testina" su clip conformate: il palliativo è in `master`
(`fd130bb`), la soluzione vera è un cambio di rappresentazione della
`Clip`, valutato e pianificato qui.

Stato: approvato nella direzione, da implementare. Nessuna riga di codice
scritta.

## Il problema

`Clip` descrive il proprio intervallo in frame **sorgente** e ne ricava la
durata di timeline:

```rust
// vv-core/src/model.rs
pub fn timeline_len(&self) -> FrameIdx {
    self.scaled(self.source_out) - self.scaled(self.source_in)
}
```

`scaled` è `rate.scale_round(...)`, con `rate = fps timeline / fps media`.
Con un media a 29,97 su timeline a 30 (`rate = 1001/1000`):

| frame sorgente | `scale_round` |
|---|---|
| 498 | 498 |
| 499 | 499 |
| 500 | **501** |

Il frame sorgente 499 copre i frame di timeline 499 **e** 500. Una clip
può cominciare o finire solo su un bordo di frame sorgente, quindi:

- **Split e trim** non possono cadere a 500: solo a 499 o 501.
- **L'audio** eredita la stessa quantizzazione: `mixer.rs` calcola
  l'offset nel buffer come `source_in / fps_media`, quindi la metà destra
  di uno split riparte dall'inizio di un frame sorgente e non dal punto
  esatto (~17 ms di salto a 29,97/30).

Con `rate = 6/5` (25 su 30) il problema tocca 1 posizione su 6.

La radice: `source_in` è un indice di frame sorgente, e non può
rappresentare una clip che inizia *a metà* di un frame sorgente.

## Perché non basta aggiungere `timeline_len`

La prima versione di questo documento proponeva di tenere `source_in`,
`source_out` e aggiungere `timeline_len`. Ha due difetti.

**1. Perde la fase.** `source_frame_at` ancora la mappatura a
`scale_round(source_in)`, cioè assume che la clip inizi su un bordo di
frame sorgente:

```rust
rate.unscale_round(t - timeline_start + rate.scale_round(source_in))
```

Split a 500 con la metà destra `source_in = 499`, `timeline_start = 500`:

| t | originale | metà destra |
|---|---|---|
| 500 | 499 | 499 |
| 501 | 500 | **499** |
| 502 | 501 | **500** |

L'intera metà destra resta indietro di un frame sorgente. Il taglio
sarebbe esatto, il contenuto no — e un test che controlla solo i bordi non
se ne accorge.

**2. Stato ridondante.** Tre valori (`source_in`, `source_out`,
`timeline_len`) che devono restare coerenti, più la fase che manca: ogni
comando presente e futuro deve aggiornarli tutti. È esattamente ciò che il
commento su `Clip::rate` voleva evitare.

## Lo standard di mercato

Nessuno memorizza in + out + durata. Lo schema comune è **punto di
ingresso + durata, in unità di tempo, non in frame del media**:

- **OpenTimelineIO**: `source_range = TimeRange(start_time, duration)`,
  entrambi `RationalTime(value, rate)`; il punto di uscita è derivato.
- **FCPXML**: `offset` / `start` / `duration` in secondi razionali.
- **Premiere**: tick (254016000000 al secondo).

## La rappresentazione proposta

```rust
pub struct Clip {
    pub timeline_start: FrameIdx,
    /// Inizio della clip nel media, in frame di *timeline* contati dal
    /// frame sorgente 0 (lo spazio di `Rational::scale_round`).
    pub source_offset: FrameIdx,
    pub timeline_len: FrameIdx,
    pub rate: Rational,
    // ...
}
```

`source_in` e `source_out` spariscono come campi e diventano metodi
derivati. Tutto è espresso nell'unità della timeline, l'unica in cui
l'utente sceglie posizioni.

| | formula |
|---|---|
| `source_frame_at(t)` | `rate.unscale_round(t - timeline_start + source_offset)` |
| `source_in()` | `rate.unscale_round(source_offset)` |
| `source_out()` (esclusivo) | `rate.unscale_round(source_offset + timeline_len - 1) + 1` |
| `timeline_end()` | `timeline_start + timeline_len` |
| `source_len()` | `source_out() - source_in()` |
| secondi nel media (audio) | `source_offset / fps_timeline` |

Per una clip non conformata (`rate = 1/1`) `source_offset == source_in` e
nulla cambia.

### Operazioni

Tutte aritmetica intera in un solo spazio, senza arrotondamenti:

- **Split a `s`**: sinistra `timeline_len = s - start`; destra
  `timeline_start = s`, `source_offset = offset + (s - start)`,
  `timeline_len = vecchia_len - (s - start)`. Le due metà condividono il
  frame sorgente a cavallo del taglio, mostrato per una parte del suo
  tempo a sinistra e per il resto a destra.
- **Trim fine a `e`**: `timeline_len = e - start`.
- **Trim inizio a `b`**: `delta = b - start`; `timeline_start += delta`,
  `source_offset += delta`, `timeline_len -= delta`.
- **Spostamento**: cambia solo `timeline_start`.

### Invarianti

Nessuno stato ridondante: non c'è niente che possa disallinearsi. Restano
solo vincoli di dominio:

- `timeline_len >= 1`
- `source_offset >= 0`
- `source_offset + timeline_len <= rate.scale_round(duration_frames)` per
  un media (nessun limite per `SolidColor`)

### Cambio di unità

`source_offset` e `timeline_len` sono in frame di timeline, quindi
dipendono dal suo fps e dall'fps del media. Serve **un** punto di
conversione, `Clip::retime(timeline_fps_vecchio, timeline_fps_nuovo,
media_fps)`, che passa per i secondi (`valore / fps_vecchio * fps_nuovo`,
arrotondato) e ricalcola `rate`. Lo usano:

- **incolla** su una timeline a fps diverso da quella di origine
  (`main.rs`, incolla da `ClipboardEntry`): va convertito anche
  `relative_start`. Oggi `rate` viene copiato così com'è, ed è già un bug
  su timeline a fps diverso;
- `refresh_clip_rates`, se l'fps del media cambia sotto i piedi (probe
  diversa, media ricollegato): si tengono `timeline_start` e
  `timeline_len` (l'intenzione di montaggio), si converte `source_offset`
  per secondi e si clampa alla durata del media;
- import OTIO (sotto).

Non è una ricodifica: il video non viene toccato, cambiano solo i numeri
della clip.

## Compatibilità con OTIO

L'import/export OTIO è tra le prossime feature: questa rappresentazione è
già la sua, quindi la mappatura è diretta.

- **Export**: `source_range.start_time = RationalTime(source_offset,
  fps_timeline)`, `duration = RationalTime(timeline_len, fps_timeline)`.
  Esatto, `RationalTime` accetta qualunque rate. `available_range` viene
  dal media (`duration_frames`, fps del media).
- **Import**: `start_time` e `duration` arrivano in un rate qualunque
  (spesso quello del media). Si portano in frame di timeline:
  - `start_time` intero nel rate del media → `rate.scale_round(value)`,
    identico a quanto fa oggi l'import di una clip;
  - altrimenti `round(secondi * fps_timeline)`.

  L'import quantizza al frame di timeline, come fanno tutti gli editor.
- **Conform**: OTIO non ha il concetto: un media a 29,97 su timeline a 30
  si riproduce a velocità reale, esattamente come il nostro `rate`. Niente
  da tradurre.
- **Fuori da questo documento, ma da tenere a mente**: le track OTIO sono
  sequenziali (`Gap` espliciti), le nostre posizionali (`timeline_start`):
  l'export deve generare i `Gap`, l'import accumularli. La speed futura
  corrisponde a `LinearTimeWarp` / `TimeEffect`.

## Impatto

**Prestazioni**: trascurabili in entrambe le direzioni. `timeline_len()`
diventa una lettura di campo; `source_in()`/`source_out()` costano una
`unscale_round` (divisione `i128`) invece di una lettura, ma sono chiamati
poche volte per clip per frame di UI. I percorsi caldi (`render_ahead`,
`export`, `mixer`) usano `source_frame_at`, che costa quanto oggi.

**Complessità**: il modello diventa più semplice (niente ridondanza,
niente scelta del bordo più vicino, niente testina da riposizionare). Il
costo è il refactor: ~89 usi di `source_out` e altrettanti di `source_in`,
quasi tutti meccanici (campo → metodo).

**Manutenzione**: un solo spazio di lavoro per i comandi di montaggio e
un solo punto di conversione di unità. Da ricordare: un nuovo comando che
crea o copia clip deve copiare `source_offset`/`timeline_len`, non
ricalcolarli da un intervallo sorgente.

**Formato di salvataggio**: cambia senza migrazione. Non esistono progetti
salvati da preservare; `#[serde(default)]` sui campi nuovi non serve.

**Comportamento**: la clip più corta passa da 1 frame sorgente a 1 frame
di timeline. Con un media molto più lento della timeline, una clip può
mostrare un frame sorgente per un solo frame di timeline. È corretto, ma
cambia rispetto a oggi.

## Piano di implementazione

Ogni passo compila e passa i test da solo.

### 1. Metodi al posto dei campi, a comportamento invariato

- Aggiungere `Clip::source_in()` / `source_out()` che per ora leggono i
  campi, e migrare tutti i lettori (vv-core, vv-app, vv-audio, vv-render)
  ai metodi. Le scritture restano sui campi.
- Aggiungere il costruttore unico `Clip::from_source_range(source,
  source_in, source_out, timeline_start, rate, ...)`: tutti i punti che
  creano una clip nuova da un media (import, drop dal media pool, test
  helper) passano da qui.

Refactor meccanico: nessun test cambia.

### 2. Cambio di rappresentazione (`vv-core/src/model.rs`)

- Sostituire i campi `source_in`/`source_out` con `source_offset` e
  `timeline_len`; i metodi del passo 1 diventano le formule della tabella.
- `from_source_range` calcola `source_offset = scale_round(source_in)`,
  `timeline_len = scale_round(source_out) - scale_round(source_in)`:
  stesso risultato di oggi per ogni clip creata da un intervallo sorgente.
- Rimuovere `source_frame_of` libera: `resolve_overlap` e
  `make_room_for_ranges` non hanno più bisogno di convertire.
- Aggiungere `Clip::retime` e usarlo in `refresh_clip_rates`.
- Aggiornare il commento su `Clip::rate`: la fonte di verità ora è
  `source_offset` + `timeline_len`.

### 3. Comandi (`vv-core/src/command.rs`)

- **`TrimClip`**: `new_value` diventa la posizione di **timeline** del
  bordo. Formule della sezione "Operazioni". `old` diventa
  `(timeline_start, source_offset, timeline_len)`.
- **`SplitClip`**: formule della sezione "Operazioni". Via la scelta del
  bordo più vicino e i suoi commenti; `original_source_out` diventa
  `original_len`. La metà destra parte da `split_at`, non da
  `clip.timeline_end()`.
- **`resolve_overlap` / `make_room_for_ranges`**: passano posizioni di
  timeline a `TrimClip`/`SplitClip`; spariscono le `source_at(...)`.

### 4. UI (`vv-app`)

- `split_at_playhead` (`main.rs`): togliere lo spostamento della testina
  sul taglio eseguito.
- `timeline_ui::single_trim_range`: i limiti sono gli invarianti di
  dominio, in frame di timeline (`timeline_start - source_offset` per
  l'inizio, `scale_round(duration_frames)` per la fine); il chiamante
  passa la posizione di timeline a `TrimClip`, senza convertirla.
- `ClipboardEntry`: `source_offset` + `timeline_len` al posto di
  `source_in`/`source_out`; all'incolla, `retime` se l'fps della timeline
  di destinazione differisce (anche `relative_start`).
- `map_source_ranges_to_timeline` (`main.rs`): usa `source_in()` /
  `source_out()`; verificare che `source_out() - 1` resti l'ultimo frame
  sorgente mostrato.

### 5. Audio (`vv-audio/src/mixer.rs`)

- `source_offset` del buffer da `clip.source_offset / fps_timeline`
  invece che da `source_in / fps_media`: l'audio segue il taglio al
  campione, non al frame sorgente.
- Gain keyframeato (`block_gain_linear`): i keyframe restano in frame
  sorgente, ma la base diventa il secondo esatto nel media
  (`source_offset / fps_timeline + secs`, poi `* fps_media`, floor) invece
  di `source_in + round(secs * fps_media)`, così coincide con il frame
  video mostrato anche dopo uno split a metà frame sorgente.

### 6. Test

- **Contenuto dopo split** (il test che scopre il bug di fase): per
  `rate` ∈ {`1/1`, `1001/1000`, `6/5`, `5/6`} e **ogni** `split_at` nel
  corpo della clip, per **ogni** `t` della clip originale,
  `source_frame_at(t)` della metà che contiene `t` è uguale a quello
  dell'originale. Sostituisce
  `split_clip_on_a_conformed_clip_cuts_at_the_nearest_source_boundary`
  (`vv-core/src/lib.rs`).
- **Split esatto**: la metà destra comincia a `split_at`, la sinistra
  finisce lì.
- **Trim esatto**: stesso schema del test di contenuto, per i due bordi.
- **Undo**: split + undo e trim + undo tornano alla clip identica.
- **Invarianti**: dopo ogni comando, `timeline_len >= 1` e i limiti del
  media rispettati.
- **`retime`**: 30 → 25 → 30 torna ai valori di partenza entro un frame;
  l'incolla su timeline a fps diverso conserva la durata in secondi.
- **Audio**: l'offset nel buffer della metà destra coincide con il
  campione che l'originale suonava a `split_at`.

Verifica manuale: export di un tratto con clip conformate e split, prima e
dopo, confrontando durata totale e punti di taglio (video e audio).

## Alternative scartate

- **Lasciare tutto com'è.** Accettabile a 29,97/30, fastidioso a 25/30.
  È lo stato attuale di `master`.
- **Aggiungere `timeline_len` accanto a `source_in`/`source_out`.** Perde
  la fase e introduce stato ridondante (vedi sopra).
- **Tempo in tick o secondi razionali (stile Premiere/FCPXML).** Più
  generale, ma ogni posizione che l'utente sceglie è comunque un frame di
  timeline: i frame di timeline bastano e restano interi. Da riconsiderare
  solo se una timeline dovrà poter cambiare fps a progetto avviato.
- **Agganciare la testina ai bordi di frame sorgente.** Passo irregolare e
  dipendente dalla clip; con più track a rate diversi non esiste un passo
  giusto.
- **Timeline sempre all'fps del media.** Rinuncia al montaggio
  multi-sorgente.

## Domande aperte

1. **Campi pubblici o privati?** Proposta: pubblici per ora (i test usano
   struct literal), con `from_source_range` come unico costruttore da
   media e un `debug_assert` degli invarianti nei `Command::apply`.
2. **`rate` resta memorizzato?** È derivabile da fps della timeline e del
   media: memorizzarlo è una cache. Tenerlo semplifica `source_frame_at`
   (nessun accesso al media pool); ricalcolarlo lo toglie dalle cose che
   `retime` deve aggiornare. Proposta: tenerlo.
3. **Speed keyframeata** (`effects.speed`): fuori da questo lavoro. La
   rappresentazione è quella giusta per affrontarla dopo (durata esplicita,
   posizione sorgente = offset + ∫speed), come in OTIO.
