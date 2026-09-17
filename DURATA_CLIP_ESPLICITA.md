# Durata di timeline esplicita nella `Clip` — valutazione e piano

Documento di discussione, **non** una decisione presa. Nasce dal bug "lo
split cade un frame prima/dopo la testina" su clip conformate: il
palliativo è in `master`, la soluzione vera è un cambio di modello che
qui viene valutato e pianificato.

Stato: da discutere. Nessuna riga di codice scritta in questa direzione.

## Il problema

`Clip` non memorizza quanto dura sulla timeline: la ricava.

```rust
// vv-core/src/model.rs
pub fn timeline_len(&self) -> FrameIdx {
    self.scaled(self.source_out) - self.scaled(self.source_in)
}
pub fn timeline_end(&self) -> FrameIdx {
    self.timeline_start + self.timeline_len()
}
```

dove `scaled` è `rate.scale_round(...)` e `rate` è `fps timeline / fps
media` (`Rational::conform_rate`). Con un media a 29,97 su una timeline a
30 il rapporto è `1001/1000`: `scale_round` avanza a scatti di 1 o 2, e
ogni ~1000 frame ne salta uno. In concreto, per `rate = 1001/1000`:

| frame sorgente | `scale_round` |
|---|---|
| 498 | 498 |
| 499 | 499 |
| 500 | **501** |

Il frame sorgente 499 copre i frame di timeline 499 **e** 500. Al frame
500 non comincia nessun frame sorgente, quindi **non esiste nessuna
coppia `(source_in, source_out)` che produca una clip lunga esattamente
500**: la durata è quantizzata sui bordi dei frame sorgente.

Conseguenza diretta: `SplitClip` non può tagliare a 500. Può tagliare a
499 o a 501, mai in mezzo. Lo stesso vale per il bordo di un trim e per
ogni altra posizione che l'utente sceglie in frame di *timeline*.

Il vincolo è deliberato, ed è scritto nel modello:

> `rate`: […] Un rapporto invece di una durata memorizzata perché
> `source_in`/`source_out` devono restare l'unica fonte di verità: un trim
> non può far divergere i due valori.

## Cosa c'è oggi in `master` (il palliativo)

Commit `fd130bb`:

1. `SplitClip` sceglie il bordo di frame sorgente **più vicino** a
   `split_at` (a parità, quello prima: così il frame che si sta guardando
   diventa il primo della metà destra invece di restare duplicato in coda
   alla sinistra).
2. `split_at_playhead` **sposta la testina sul taglio davvero eseguito**,
   così la linea rossa e il taglio coincidono sempre: l'utente vede la
   testina scattare di un frame, invece di vedere il taglio dove la linea
   non è.

Non è una correzione: è rendere onesto un limite. L'errore resta ≤ 1
frame e si manifesta in ~1 posizione su 1000 con media 29,97 su timeline
30 — ma su `rate = 6/5` (media 25 su timeline 30) sarebbe 1 posizione su
6, molto più visibile.

## La modifica proposta

Memorizzare la durata di timeline nella `Clip` invece di derivarla:

```rust
pub struct Clip {
    pub source_in: FrameIdx,
    pub source_out: FrameIdx,
    pub timeline_start: FrameIdx,
    pub timeline_len: FrameIdx, // <-- nuovo, non più derivato
    pub rate: Rational,
    // ...
}
```

`source_in`/`source_out` continuano a dire *quale* materiale si vede,
`timeline_len` dice *per quanto* lo si vede. I due valori possono
divergere di una frazione di frame, ed è esattamente ciò che serve per
tagliare a 500.

`source_frame_at` (timeline → sorgente) **non cambia**: resta basata su
`rate`. Cambia solo il fatto che l'ultimo frame sorgente della clip può
essere mostrato per un frame di timeline invece che per due.

## Impatto sulle prestazioni

**Trascurabile, e semmai positivo.**

- `timeline_len()` oggi costa due moltiplicazioni/divisioni `i128`
  (`scale_round` due volte). Diventerebbe una lettura di campo.
- È chiamata da ~26 punti, quasi tutti nel disegno della timeline e nei
  test di sovrapposizione (`neighbor_bounds_at`, `clips_intersecting_rect`,
  `active_clip_at`, il loop di disegno delle clip): più volte per clip per
  frame di UI. Sono comunque poche centinaia di operazioni intere per
  frame, contro decode e compositing video: **non misurabile in nessuna
  delle due direzioni**.
- Nessun impatto sui percorsi caldi veri (`render_ahead`, `export`,
  `mixer`): usano `source_frame_at`, che non cambia.
- Memoria: 8 byte per clip. Irrilevante.

**La performance non è il criterio di decisione.** Il costo vero è la
manutenzione dell'invariante: `timeline_len` diventa stato che può
disallinearsi da `source_in`/`source_out`/`rate`, mentre oggi
l'incoerenza è impossibile per costruzione.

## Rischi

| Rischio | Dove | Mitigazione |
|---|---|---|
| Comando che dimentica di aggiornare `timeline_len` → clip lunga o corta a caso | `TrimClip`, `SplitClip` | i due punti sono in `command.rs:611-647` e `846-920`, il resto crea clip nuove; un test di invariante li copre |
| Undo che ripristina il sorgente ma non la durata | `TrimClip::undo`, `SplitClip::undo` | `old`/`original_source_out` diventano tuple che includono la durata |
| Progetti salvati prima del campo | `persistence.rs` | `#[serde(default)]` + ricalcolo al caricamento, esattamente come già fatto per `rate` con `refresh_clip_rates` |
| Cambio di `rate` a progetto caricato (`refresh_clip_rates`) che non tocca la durata | `model.rs:845` | decidere esplicitamente: la durata salvata vince, oppure viene ricalcolata |
| Ultimo frame sorgente mostrato per un frame invece di due | `render_ahead`, `export`, `frame_provider` | già clampano, ma va verificato con un export di confronto |
| Durata e sorgente che divergono all'infinito dopo molti trim | ovunque | i trim scrivono *entrambi* i valori dalla stessa posizione di timeline, non incrementalmente |

## Piano di implementazione

Sei passi, ognuno compilabile e testabile da solo.

### 1. Campo e costruzione (`vv-core/src/model.rs`)

- Aggiungere `timeline_len: FrameIdx` a `Clip`, con `#[serde(default)]`.
- `timeline_len()` diventa un getter del campo; `timeline_end()` resta
  `timeline_start + timeline_len`.
- Aggiungere `Clip::conformed_len(source_in, source_out, rate)` (l'attuale
  formula derivata) come *unico* punto che calcola una durata da un
  intervallo sorgente: lo usano tutti i costruttori di clip.
- `source_len()` resta com'è.

Alla fine di questo passo il campo esiste ma vale sempre quanto la
formula: nessun comportamento cambia.

### 2. Caricamento dei progetti (`vv-core/src/persistence.rs`)

- `load_project` chiama già `refresh_clip_rates`; aggiungere accanto un
  `refresh_clip_lengths` che riempie `timeline_len` **solo se è 0** (il
  default serde), calcolandolo con `conformed_len`.
- Decidere e documentare cosa fa `refresh_clip_rates` quando l'fps del
  media cambia sotto i piedi: oggi ricalcola `rate` e quindi la durata
  cambia da sola. Con la durata memorizzata bisogna scegliere se la clip
  mantiene la durata (e slitta il contenuto) o la ricalcola. **Proposta:
  ricalcolarla**, perché è un caso di "riallineamento al media reale", non
  un'intenzione di montaggio.

### 3. Trim (`vv-core/src/command.rs`, `TrimClip`)

È il punto centrale. `TrimClip::new_value` è un frame *sorgente*; il
chiamante (`timeline_ui`) parte però da un frame di *timeline* e lo
converte con `source_frame_at`, perdendo lì la precisione.

- Portare `TrimClip` a ricevere **anche** la posizione di timeline del
  bordo (`new_timeline_value`), o a riceverla al posto del frame sorgente
  e derivarsi il sorgente da sé.
- `TrimEdge::End`: `source_out = source_frame_at(bordo)`,
  `timeline_len = bordo - timeline_start`.
- `TrimEdge::Start`: la fine resta ferma → `source_in = source_frame_at(bordo)`,
  `timeline_start = bordo`, `timeline_len = vecchia fine - bordo`.
- `old` diventa `(source_in, source_out, timeline_start, timeline_len)`.

Da qui il bordo di un trim cade esattamente dove l'utente lo lascia, e
sparisce anche il clamp "almeno 1 frame" espresso in frame sorgente.

### 4. Split (`vv-core/src/command.rs`, `SplitClip`)

- Metà sinistra: `source_out = source_frame_at(split_at)` (il frame che
  *copre* `split_at`, cioè il `floor`), `timeline_len = split_at - timeline_start`.
- Metà destra: `source_in` = lo stesso frame sorgente (è quello che si
  vede a `split_at`), `timeline_start = split_at`,
  `timeline_len = vecchia fine - split_at`.
- Rimuovere la scelta del bordo più vicino e il commento che la spiega:
  non serve più.
- `undo` ripristina `source_out` **e** `timeline_len` della sinistra.

Nota: le due metà condividono un frame sorgente (quello a cavallo del
taglio). È corretto — è lo stesso frame, mostrato per una parte del suo
tempo a sinistra e per il resto a destra.

### 5. Testina e UI (`vv-app`)

- `split_at_playhead`: togliere lo spostamento della testina sul taglio
  (`main.rs`, blocco "Il taglio può cadere un frame più in là"): non ci
  sarà più niente da compensare.
- `timeline_ui::single_trim_range`: i limiti restano quelli di oggi
  (sorgente e bordo opposto), ma il minimo "almeno 1 frame di contenuto"
  si esprime ora in frame di timeline.
- `make_room_for_ranges`/`resolve_overlap` (`command.rs`): usano
  `TrimClip` e `SplitClip` con posizioni di *timeline* — vanno adeguati
  alla firma nuova, e diventano più semplici (spariscono le conversioni
  `source_at(...)`).

### 6. Verifica

Test nuovi:

- **Invariante**: dopo un trim/split qualsiasi, `timeline_end()` della
  metà sinistra `==` `timeline_start` della destra (già coperto), e
  `timeline_len > 0` per ogni clip.
- **Split esatto**: su clip conformata `rate = 1001/1000` e `rate = 6/5`,
  per *ogni* `split_at` nel corpo della clip, la metà destra comincia
  esattamente a `split_at`. È il test che oggi fallisce e che tollera ±1
  (`vv-core/src/lib.rs`, `split_clip_on_a_conformed_clip_cuts_at_the_nearest_source_boundary`):
  va stretto a uguaglianza e rinominato.
- **Trim esatto**: stesso schema per i due bordi.
- **Copertura**: split + undo riporta alla durata originale; split di una
  clip conformata copre esattamente l'originale (test già esistente).
- **Round-trip**: salva/carica un progetto con clip conformate e durate
  non derivabili, e verifica che le durate sopravvivano.

Verifica manuale: un export di un tratto con clip conformate prima e dopo
la modifica, confrontando durata totale e punti di taglio.

## Alternative scartate

- **Lasciare tutto com'è.** Accettabile per media 29,97 su timeline 30
  (1 posizione su 1000), fastidioso per 25 su 30 (1 su 6). È lo stato
  attuale di `master`.
- **Agganciare la testina ai bordi di frame sorgente.** La linea non
  potrebbe mai fermarsi dove non si può tagliare, quindi taglio e linea
  coinciderebbero sempre. Ma il passo della testina diventerebbe
  irregolare e dipendente dalla clip sotto di essa — peggio, con più clip
  a rate diversi sotto track diverse non esiste un unico passo giusto.
- **Timeline sempre all'fps del media.** Elimina il problema alla radice
  ma rinuncia al montaggio multi-sorgente, che è il punto dell'editor.

## Domande aperte

1. La durata memorizzata sopravvive a un cambio di fps del media
   (`refresh_clip_rates`), o viene ricalcolata? (proposta: ricalcolata)
2. `timeline_len` pubblico e scrivibile, o privato con setter che
   impediscano lo zero/negativo?
3. Vale la pena estendere lo stesso trattamento alla *speed* keyframeata
   (`effects.speed`), che oggi non entra nel calcolo della durata?
