# REFACTOR_PIPELINE — riscrittura della pipeline di rendering

Documento di lavoro per l'implementazione. Unisce due valutazioni
dell'architettura attuale (entrambe concorde nel verdetto: **non è la
migliore possibile "a prescindere da cosa c'è sulla timeline"**; i bug
recenti nascono in `RenderAhead` da regole ad-hoc che interagiscono) e le
trasforma in un piano d'intervento ordinato per rischio/beneficio.

Nessuna riga di codice in questo doc: è la spec a cui implementare.

---

## 0. Scope: due parti, una priorità

- **Part A — riscrittura di `RenderAhead` + cache.** Qui vivono i bug che
  stiamo fixando da più sessioni (sfratto che si ricalcola da capo, buffer
  che "sembrava per-clip", seek in loop). È la priorità: chiudere la *classe*
  di bug, non il sintomo.
- **Part B — pipeline più ampia** (duplicazione preview/export, single-track
  hardcodato, round-trip GPU del compositor, scelta CPU-RGBA, audio per-clip).
  Strutturale e importante, ma **non blocca A**: si fa a cache stabile, un
  pezzo alla volta.

Il principio trasversale (vedi §5): l'accuratezza del frame è non
negoziabile — nessuna ottimizzazione può mostrare un frame approssimativo o
stale per guadagnare fluidità.

---

## 1. Diagnosi condivisa (sintesi dei due punti di vista)

| # | Problema | Dove | Perché è radice, non sintomo | Fonte |
|---|----------|------|------------------------------|-------|
| A1 | Due meccanismi di sfratto scollegati (`evict_before` posizionale + LRU per capacità) che rispondono alla stessa domanda "cosa tenere in cache" e possono contraddirsi | `cache.rs`, `render_ahead.rs` | Bug #4: la capacità sfrattava ciò che `evict_before` avrebbe protetto. Non è l'ultimo scontro possibile | collega |
| A2 | Budget diviso a metà tra media distinti, poi di nuovo a metà tra segmenti dello stesso media — mai proporzionale al bisogno; `resize` a ogni ciclo anche quando nulla cambia per quel media | `render_ahead.rs` (`per_media_budget`, `segments_per_media`, `cache.resize`) | La qualità del buffer di una clip dipende da quante clip *estranee* capitano vicine: accoppiamento indebito | collega |
| A3 | `SEEK_THRESHOLD_FRAMES = 120` numero magico non misurato; il costo reale di un seek dipende dal GOP/container | `render_ahead.rs:51` | Sbagliato di un ordine di grandezza tra 4K long-GOP e 1080p intra-friendly; non si adatta | collega |
| A4 | Un solo worker seriale; nessuna priorità esplicita tra "questo frame serve ORA" e "prefetch, può aspettare". Funziona solo perché il primo segmento parte sempre dalla testina | `render_ahead.rs` (worker singolo) | Più media distinti nella finestra → più lento a mettersi in pari, in scala lineare | collega |
| B1 | Due pipeline di rendering duplicate: la mappatura clip→frame-sorgente è scritta due volte (`render_ahead.rs:276` e `export.rs:181-182`) con strategie diverse (cache vs streaming) | `render_ahead.rs`, `export.rs` | Oggi coincidono solo perché `speed==1`; allo speed-change (milestone 7) divergono, nessun posto unico per il time-remap | mio |
| B2 | Modello dati N-track generico, rendering single-track: `VIDEO_TRACK=0`/`AUDIO_TRACK=1` hardcodati; compositor a una sola texture input, blend `REPLACE`, niente accumulo multi-traccia | `render_ahead.rs:35`, `export.rs:22-23`, `compositor.rs` | "A prescindere dalla timeline" è falso per costruzione: una 2ª track video non viene né bufferizzata né compositata | mio + nota collega |
| B3 | Compositor con round-trip CPU→GPU→CPU bloccante a ogni frame (upload → render → `copy_texture_to_buffer` → `map_async`+`poll(wait_indefinitely)` → readback → ricarica in egui) | `compositor.rs:277-306` | Tetto di latenza sul thread UI; la GPU è usata come *filtro pixel*, non come *destinazione* | mio |
| B4 | Conversione YUV→RGB su CPU (`sws_scale`) → frame RGBA8 (4 B/px) invece di YUV420 (~1.5 B/px): ~2.5× meno frame in cache a parità di RAM, e forza il round-trip B3 | `decode.rs:19-24` | Degrada copertura buffer *e* latenza insieme; scelta "provvisoria" che si è fossilizzata | mio |
| B5 | Audio per-clip (un `AudioPlayer` per clip attiva, scambiato al taglio) invece di mixer continuo; `vv-audio::mixer` ancora scheletro | `player.rs:33` | Sync tra tagli affidato a riuso ad-hoc (`audio_cache`); non si estende a N track audio | mio |

**Nota di conciliazione.** A1+A2 (collega) e i miei punti sul buffer sono la
stessa modifica vista da due angoli: un unico pass di sfratto + budget
globale elimina *contemporaneamente* lo scontro tra i due meccanismi (A1) e i
muri artificiali del budget diviso (A2). Non sono due fix separati.

---

## 2. Architettura target della cache (Part A — il pezzo grosso)

### Da dove partiamo (stato attuale, `cache.rs` + `render_ahead.rs`)
- N `FrameCache` indipendenti, una per `MediaId`, in un `HashMap`.
- Ogni cache: `lru::LruCache<FrameIdx, Arc<FrameRgba>>` — **chiave solo
  `FrameIdx`**, non composita (la chiave `(MediaId, SourceFrameIdx)` prevista
  in ARCHITECTURE.md non è mai arrivata).
- Budget diviso: `per_media_budget = budget / media_distinti`, poi
  `per_segment_capacity = capacity / segmenti_del_media`.
- Tre operazioni di sfratto separate per ciclo: `retain` (media usciti dalla
  finestra), `evict_before(min_source_start)` (dietro la testina), e LRU per
  capacità (al `resize`/`insert`).
- `capped_source_end`: non decodificare oltre la capacità per-segmento.

### Dove arriviamo (target)
**Una sola cache a chiave composita `(MediaId, SourceFrameIdx)`, un budget
globale in byte, e un unico pass `reconcile()` che decide lo sfratto con una
politica a priorità per distanza dalla testina.**

La metrica di priorità è **`distanza_dalla_testina`**: per ogni frame in
cache `(media, source_idx)`, si calcola la sua posizione in *spazio timeline*
(via il segmento della finestra corrente che lo contiene: mappatura
source→timeline) e poi `|posizione_timeline − playhead|`. Il frame più vicino
alla testina ha priorità massima; un frame fuori da qualsiasi segmento
corrente ha distanza infinita (candidato di sfratto immediato).

> Questa singola metrica risolve insieme A2 (il budget va a ciò che è più
> vicino/serve, non equal-split) e A4 (il "frame che serve ORA" è per
> costruzione il più prioritario), ed è la generalizzazione dell'idea
> "sfratto basato su finestra + direzione".

### `reconcile()` — un solo pass, due tier ordinati

```rust
/// Ricalibra l'intera cache in UN pass, chiamato una volta a ogni ciclo di
/// poll dopo aver aggiornato playhead e i segmenti della finestra.
/// Sostituisce le tre operazioni separate odierne (retain + evict_before +
/// LRU-per-capacità) con una politica unica.
fn reconcile(
    &mut self,
    playhead: FrameIdx,          // testina in frame di timeline
    window: &[MediaSegment],     // segmenti in [playhead, playhead+lookahead]
                                 // (estesi con il range timeline di ciascuno)
    budget_bytes: usize,         // budget RAM GLOBALE, non diviso
) {
    // TIER A — finestra/posizionale.
    // Scarta ogni (media, idx) fuori dall'unione degli intervalli sorgente
    // dei segmenti correnti di quel media. Un'unica regola copre:
    //   - frame dietro la testina  (ex evict_before),
    //   - media usciti dalla finestra (ex retain),
    //   - porzioni oltre l'orizzonte di lookahead (ex capped_source_end).
    // → elimina come stato separato: min_source_start_by_media, retain,
    //   per_segment_capacity/capped_source_end.

    // TIER B — budget globale in byte.
    // Se bytes_usati > budget_bytes: sfratta i frame IN-finestra più LONTANI
    // dalla testina (NON i meno recenti!) finché sotto budget.
    // → elimina: divisione per-media/per-segmento e resize a caldo.
}
```

**⚠️ Sottigliezza critica (dove un'implementazione ingenua reintroduce un bug).**
Il Tier B **non può usare LRU-per-recency**. Durante il fill in avanti, il
frame *alla testina* è il primo inserito = il meno recente: una LRU classica lo
sfratterebbe per primo, esattamente l'opposto di ciò che serve. Il Tier B deve
ordinare per `distanza_dalla_testina` crescente e sfrattare dalla fine (il più
lontano davanti). La recency può restare solo come *tiebreaker* tra frame a
parità di distanza.

**Nota sulla struttura dati.** `lru::LruCache` capisce in *numero di elementi*,
non in byte. Per un budget in byte: si tiene un contatore `bytes_usati` accanto
alla cache e il Tier B poppa voci (ordinate per distanza) finché
`bytes_usati ≤ budget_bytes`. La chiave composita `(MediaId, FrameIdx)` rende
naturale un solo pass globale; l'alternativa (N mappe per-media guidate da un
passo globale unico) è equivalente ma più codice — si consiglia la chiave
composita.

### Cosa sparisce dal codice (superficie ad-hoc eliminata)
- `min_source_start_by_media` (stato separato) → dentro Tier A.
- `segments_per_media`, `per_segment_capacity`, `capped_source_end` → sostituiti
  da budget globale + ordine di fill per priorità.
- `cache.resize()` a caldo e la divisione `per_media_budget` → un solo budget
  globale.
- `evict_before` come *metodo separato* → la sua regola diventa il Tier A.
  **Attenzione:** si elimina il *meccanismo separato*, NON l'intenzione
  posizionale — che deve restare (altrimenti regredisce "fronte del buffer ≠
  testina").

### L'ordine di fill (lato decode, non solo sfratto)
Il worker decodifica **in ordine di priorità per distanza dalla testina,
attraverso tutti i media della finestra**, fino a saturare il budget globale o
coprire la finestra. Non "riempi il media A fino in fondo poi il media B"
(che sarebbe first-come-first-served, l'arbitrario al posto dell'equal-split).
In pratica: il frame sotto la testina (quale che sia il suo media) viene prima,
poi i più vicini. Questo fa sì che, anche con un solo worker seriale, la clip
attiva non mai attenda dietro a un prefetch lontano — risponde ad A4 senza
servire ancora il pool multi-worker.

Il check di **riconnessione** esistente (fermarsi quando si ricongiunge a frame
già in cache, guardando sia `next_frame` sia la fine del segmento) resta: è ciò
che fa sì che uno scrub piccolo all'indietro non ridecodifichi l'intera coda.

### Invarianti che devono valere (da testare)
1. **Fronte alla testina:** a regime, il buffer inizia (o subito dopo) la
   testina corrente, anche avanzando a piccoli passi senza seek reale.
2. **Stabilità con testina ferma:** nessun ciclo ricalcola/invalida ciò che è già
   corretto; il range attorno alla testina non sparisce e riappare.
3. **Nessun segmento invalida l'altro:** due segmenti dello stesso media nella
   stessa finestra (taglio tra due pezzi del file) non si sfrattano a vicenda,
   nemmeno con budget stretto.
4. **Copertura sotto budget piccolo:** con budget insufficiente per tutta la
   finestra, il buffer parte comunque dalla testina, non da una coda arbitraria.
5. **Catching-up agli scrub indietro** (piccoli e grandi): il buffer raggiunge la
   nuova posizione; uno scrub piccolo non ridecodifica la coda già in cache.
6. **Seek riusato:** un riposizionamento di un media già aperto usa
   `seek_to_time` sul decoder esistente, mai una riapertura da `Decoder::open`.

### Test di regressione esistenti — LA rete di sicurezza
Sono in `crates/vv-app/src/render_ahead.rs`. I loro **comportamenti** devono
continuare a passare; i *nomi* che riferiscono interni rimossi (es.
`..._via_capacity_when_the_budget_is_tight`, `..._within_the_old_threshold`)
si rinominano/rifattorizzano, ma la copertura del comportamento non si toglie:

- `walk_and_fill_keeps_the_buffer_front_at_the_playhead_even_without_a_real_reseek` → invariante 1
- `render_ahead_does_not_loop_when_the_playhead_sits_still_just_before_a_cut` → invariante 2
- `walk_and_fill_does_not_invalidate_one_segment_while_processing_another_segment_of_the_same_media` → invariante 3
- `walk_and_fill_does_not_let_one_segment_of_a_media_evict_another_via_capacity_when_the_budget_is_tight` → invariante 3 (questo è il test del bug #4)
- `walk_and_fill_prioritizes_frames_near_the_playhead_when_the_budget_is_too_small_for_the_full_window` → invariante 4
- `render_ahead_catches_up_after_a_large_backward_seek`, `..._small_backward_seek_within_the_old_threshold`, `walk_and_fill_catches_up_after_a_backward_seek_above_the_historical_minimum` → invariante 5
- `walk_and_fill_does_not_redecode_the_already_buffered_tail_after_a_small_backward_seek` → riconnessione
- `position_decoder_reuses_the_open_decoder_for_a_real_seek_instead_of_reopening_the_file`, `..._does_not_reseek_when_already_usefully_ahead...`, `..._does_not_reseek_across_cycles_when_the_same_media_appears_in_two_segments` → invariante 6

---

## 3. Ordine di intervento — Part A (i due piani conciliati)

Ordinato per rischio crescente e dipendenze. **Un passo alla volta**; dopo
ciascuno: `cargo build -p vv-app` + test, e nessun passo ne presuppone un altro
non ancora fatto.

1. **Reattività del loop di decodifica.** Il loop che decodifica un segmento
   rilegge il target (atomico) ogni N frame e interrompe il segmento appena
   diventa obsoleto (testina saltata). Piccolo, indipendente, rischio ~0,
   beneficio di reattività immediato. *Non* tocca la politica di sfratto: si
   limita a non sprecare lavoro su un prefetch che non serve più. (collega #1)

2. **Cache unificata + `reconcile()` a due tier + budget globale + chiave
   composita + fill per priorità distanza.** Il pezzo grosso (§2). Chiude per
   costruzione la classe di bug A1+A2, e posiziona correttamente per A4. È la
   modifica più grande: va dopo il passo 1 (modello sfratto stabile prima di
   toccare altro) e prima del pool. (collega #2 = miei punti sul buffer)

3. **Soglia di seek adattiva sul GOP osservato.** Dopo ogni seek reale si
   conosce l'indice del keyframe su cui il decoder è atterrato; da due
   atterraggi consecutivi si stima il GOP reale e la soglia diventa "circa un
   GOP", non 120 fissi. Fallback conservativo basso (~25–30) finché non c'è
   osservazione — sbagliare per eccesso ora costa poco perché i seek riusano il
   decoder già aperto. Piccolo, indipendente, bassa priorità. (collega #3)

4. **Pool di worker con coda a priorità per decode parallelo.** Il più grande e
   rischioso: concorrenza reale (race, coordinamento priorità, cache condivisa
   da più scrittori) sopra un modello appena raddrizzato. Si fa **ultimo**, a
   modello-cache stabile. La buona notizia: la coda a priorità *è* l'ordine di
   fill per distanza già introdotto al passo 2 — il pool lo distribuisce su N
   thread invece di eseguirlo in serie. (collega #4)

---

## 4. Part B — pipeline più ampia (non blocca A; a cache stabile)

Tracciati, ordinati per impatto sul "playback a prescindere dalla timeline".
Ognuno è indipendente da A e tra loro salvo dove indicato.

- **B1 — Unificare l'acquisizione del frame.** Una funzione condivisa
  `source_frame_for(clip, timeline_frame, speed) -> FrameIdx` + un trait
  `FrameProvider` implementato sia da preview (backed dalla cache di Part A) che
  da export (streaming). Elimina la duplicazione B1 e rende lo speed-change
  (milestone 7) **un solo** cambio invece di due. Fatto *prima* della milestone 7,
  altrimenti si paga il debito due volte. *(mio #1)*

- **B2 — Togliere il round-trip CPU→GPU→CPU del compositor.** Condividere la
  texture di output direttamente con `egui-wgpu` (niente readback), oppure fare
  crop/zoom su CPU finché non serve davvero il multi-track GPU. Il singolo win
  di latenza più grande, indipendente da A. *(mio #4)*

- **B3 — Riconsiderare YUV-vs-RGBA.** Restare in YUV: cache ~2.5× più capiente a
  parità di RAM *e* abilita B2 senza readback (conversione YUV→RGB in shader).
  **Vincolo: NON farlo durante la riscrittura della cache di Part A** — cambiare
  formato frame mentre si cambia politica di sfratto confonde il debug. È un
  passo combinato con B2, dopo A. *(mio #5)*

- **B4 — Generalizzare a N track.** Togliere le costanti `VIDEO_TRACK`/
  `AUDIO_TRACK`; compositor multi-input con blend-over track-per-track (bottom→top)
  come descritto in ARCHITECTURE.md §Compositing step 6. Rende strutturalmente
  vero "a prescindere dalla timeline". *(mio #2 + nota collega)*

- **B5 — Mixer audio continuo.** Ring buffer che somma le clip attive (invece di
  uno swap per-clip con riuso ad-hoc). Sync più robusto e si estende a N track
  audio. Riutilizza `vv-audio::mixer` oggi scheletro. *(mio #6)*

Ordine consigliato Part B: **B1 → B2 → (B3 combinato) → B4 → B5**. B1 prima di
tutto perché è il prerequisito della milestone 7 e a costo contenuto; B2 per il
win di latenza; B3 agganciato a B2; B4/B5 quando serve davvero il multi-track.

---

## 5. Vincoli trasversali (non negoziabili)

- **Accuratezza del frame.** Mai mostrare un frame approssimativo/stale per
  guadagnare fluidità. Ogni ottimizzazione deve preservare: *il frame mostrato
  è esattamente quello della timeline al playhead*. Se una proposta di perf
  richiede questo trade-off, si ferma e si chiede prima.
- **Un cambio alla volta.** Dopo ogni passo: `cargo build -p vv-app` (non solo
  check) + suite di test. Nessun passo presuppone un successivo.
- **I test di regressione sono la rete di sicurezza.** Si estendono, non si
  tolgono. Un test che smette di passare dopo un refactoring è un segnale che il
  refactoring ha perso un invariante — si indaga, non si "aggiusta" il test.
- **Niente casi speciali per scenario.** Il punto di partenza dell'intero
  refactor è che `RenderAhead` cammina la timeline *senza* ramificazioni per
  vuoto/taglio/stesso-media. La riscrittura deve mantenere questa proprietà:
  un solo cammino uniforme, non una lista di if per scenario.

---

## 6. Riferimenti file

- `crates/vv-app/src/render_ahead.rs` — worker, `walk_and_fill`,
  `position_decoder`, `collect_media_segments`; test di regressione. **(Part A)**
- `crates/vv-media/src/cache.rs` — `FrameCache`: da riscrivere/estendere a chiave
  composita + `reconcile()`. **(Part A)**
- `crates/vv-media/src/decode.rs` — `Decoder`, `FrameRgba` (formato frame: B3).
- `crates/vv-app/src/export.rs` — `export_timeline`, `ActiveClipDecoder`,
  `render_video_frame` (mappatura duplicata: B1).
- `crates/vv-render/src/compositor.rs` — `render_frame` round-trip (B2),
  single-input/REPLACE (B4).
- `crates/vv-app/src/player.rs` — `Player` audio per-clip (B5).
- `crates/vv-audio/src/mixer.rs` — scheletro da implementare (B5).
