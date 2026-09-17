# REFACTOR_PIPELINE — regole e stato della pipeline di rendering

Il refactor della pipeline è quasi concluso: resta solo il pool di worker
(§3.4). Questo documento tiene gli invarianti e i vincoli che il codice deve
continuare a rispettare, più gli identificativi (A1…B5, §x) citati nei
commenti del codice. La descrizione dell'architettura attuale è in
[ARCHITECTURE.md](ARCHITECTURE.md); il vecchio codice e la diagnosi
originale sono nella git history.

---

## 1. Problemi di partenza e stato

| # | Problema risolto | Soluzione attuale |
|---|------------------|-------------------|
| A1 | Due meccanismi di sfratto scollegati (posizionale + LRU per capacità) che si contraddicevano | Un solo pass `SharedFrameCache::reconcile` (§2) |
| A2 | Budget diviso tra media e segmenti, non proporzionale al bisogno | Budget globale in byte, sfratto per distanza dalla testina (§2) |
| A3 | Soglia di seek fissa, non adatta al GOP reale | Soglia adattiva sul GOP osservato (§3.3) |
| A4 | Un solo worker seriale senza priorità tra "serve ora" e prefetch | Fill in ordine di distanza dalla testina; il pool multi-worker resta da fare (§3.4) |
| B1 | Mappatura clip→frame sorgente duplicata tra anteprima ed export | `Clip::source_frame_at` + trait `FrameProvider` |
| B2 | Round-trip CPU→GPU→CPU del compositor a ogni frame di anteprima | `Compositor::render_frame_to_texture` registrato in `egui-wgpu` |
| B3 | Frame in cache in RGBA convertiti su CPU | Frame YUV420 in cache, conversione nello shader |
| B4 | Rendering limitato a una track video e una audio fisse | N track video (vince la più in alto) e N track audio (sommate) |
| B5 | Audio per-clip invece di un mixer continuo | `vv_audio::Mixer`: somma tutte le track ed è il clock del playback |

---

## 2. Cache dei frame (Part A)

**Una sola cache a chiave composita `(MediaId, frame sorgente)`, un budget
globale in byte, e un unico pass `reconcile()` che decide lo sfratto** con
priorità per distanza dalla testina: per ogni frame si calcola la posizione
in spazio timeline (tramite il segmento della finestra che lo contiene) e poi
`|posizione − playhead|`; un frame fuori da ogni segmento ha distanza infinita.

- **Tier A (finestra):** via ogni frame fuori dall'unione degli intervalli
  sorgente dei segmenti correnti del suo media. Copre dietro la testina,
  media usciti dalla finestra e oltre l'orizzonte di lookahead.
- **Tier B (budget):** se sopra budget, via i frame in finestra **più
  lontani** dalla testina.

**⚠️ Il Tier B non può usare LRU per recency.** Durante il fill in avanti il
frame alla testina è il primo inserito, quindi il meno recente: una LRU lo
sfratterebbe per primo. La recency può fare solo da tiebreaker a parità di
distanza.

**Ordine di fill.** Il worker decodifica in ordine di distanza dalla testina
attraverso tutti i media della finestra, fino al budget o alla copertura
della finestra: il frame che serve ora arriva sempre prima di un prefetch
lontano. Il fill si ferma quando si ricongiunge a frame già in cache, così
uno scrub piccolo all'indietro non ridecodifica la coda.

### Invarianti (coperti dai test in `render_ahead.rs` e `vv-media/src/cache.rs`)
1. **Fronte alla testina:** a regime il buffer parte dalla testina corrente,
   anche avanzando a piccoli passi senza seek reale.
2. **Stabilità con testina ferma:** nessun ciclo invalida ciò che è già
   corretto.
3. **Nessun segmento invalida l'altro:** due segmenti dello stesso media nella
   stessa finestra non si sfrattano a vicenda, nemmeno con budget stretto.
4. **Copertura sotto budget piccolo:** il buffer parte comunque dalla testina.
5. **Scrub all'indietro** (piccoli e grandi): il buffer raggiunge la nuova
   posizione; uno scrub piccolo non ridecodifica la coda già in cache.
6. **Seek riusato:** riposizionare un media già aperto usa `seek_to_time` sul
   decoder esistente, mai una riapertura.

---

## 3. Passi della Part A

1. ✅ **Reattività del fill:** il loop rilegge il target ogni N frame e
   interrompe un prefetch diventato obsoleto.
2. ✅ **Cache unificata + `reconcile()` + budget globale + fill per distanza** (§2).
3. ✅ **Soglia di seek adattiva:** dopo ogni seek reale si stima il GOP dai
   keyframe di atterraggio; la soglia è circa un GOP, con un fallback basso
   finché non c'è un'osservazione (sbagliare per eccesso costa poco perché il
   seek riusa il decoder aperto).
4. ⏳ **Pool di worker con coda a priorità:** distribuire su N thread lo
   stesso ordine di fill per distanza. Da fare solo se un thread non tiene il
   passo; è il passo più rischioso (concorrenza sulla cache condivisa).

---

## Proxy

Copia tutto-intra a bassa risoluzione di ogni media, generata in background:
rende ogni frame raggiungibile con un decode singolo invece di attraversare il
GOP del sorgente, quindi lo scrub veloce tiene il passo. Non è
un'approssimazione: il frame mostrato resta quello esatto alla posizione
richiesta. L'export usa sempre i sorgenti originali. Dettagli in ARCHITECTURE.md.

---

## 5. Vincoli trasversali (non negoziabili)

- **Accuratezza del frame.** Mai mostrare un frame approssimativo/stale per
  guadagnare fluidità: *il frame mostrato è esattamente quello della timeline
  al playhead*. Se una proposta di perf richiede questo trade-off, ci si ferma
  e si chiede prima.
- **Un cambio alla volta.** Dopo ogni passo: `cargo build -p vv-app` (non solo
  check) + suite di test.
- **I test di regressione sono la rete di sicurezza.** Si estendono, non si
  tolgono. Un test che smette di passare dopo un refactoring segnala un
  invariante perso: si indaga, non si "aggiusta" il test.
- **Niente casi speciali per scenario.** `RenderAhead` cammina la timeline
  senza ramificazioni per vuoto/taglio/stesso media: un solo cammino uniforme.
