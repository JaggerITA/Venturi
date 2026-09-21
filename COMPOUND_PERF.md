# COMPOUND_PERF — piano per il rendering delle compound clip

Diagnosi e piano nati dal problema riportato: su una timeline 1080@60, un
insieme di clip che a velocità normale renderizza in tempo reale (e regge
>1x) non sta più dietro al playback a 1x appena viene raggruppato in una
compound clip.

Complemento di [REFACTOR_PIPELINE.md](REFACTOR_PIPELINE.md) (§2 cache, B2/B3
compositing); l'architettura corrente è in [ARCHITECTURE.md](ARCHITECTURE.md).

---

## 1. Dov'è il costo, oggi

Per **ogni** frame di compound clip, `render_ahead::compose_frame_at`:

1. upload dei piani YUV dei layer annidati sul device **headless** del
   worker — un secondo device wgpu, separato da quello di egui che usa la UI
   (`main.rs`, `Compositor::new_headless` nel worker vs `Compositor::new`
   con `wgpu_render_state`);
2. render pass → texture RGBA;
3. `Compositor::read_rgba_texture`: `copy_texture_to_buffer` di 8.3 MB +
   `map_read` con `poll(wait_indefinitely)` → **stallo sincrono della GPU a
   ogni frame**, nessun pipelining. Alloca anche un buffer di readback nuovo
   a ogni chiamata (il path i420 invece lo poola);
4. de-padding delle righe: altri 8.3 MB di memcpy;
5. `frame_provider::rgba_to_yuv420_with_alpha`: **loop scalare su CPU** su
   2.07M px (luma) + 518k blocchi 2x2 con divisioni (croma) + piano alpha.
   Single-thread. Da solo sfora il budget di 16.6 ms a 60 fps;
6. cache: 3.1 MB YUV + 2.07 MB di alpha → un frame compound pesa ~1.6x un
   frame normale, quindi **riduce il lookahead di tutti gli altri media** a
   parità di budget;
7. thread UI: ri-upload di 4 piani sul device di egui + `yuv_to_rgb` nello
   shader.

In sintesi: **due round-trip completi RGB↔YUV e un readback bloccante** che
le clip non raggruppate non pagano, il tutto serializzato col decode nello
stesso thread del worker.

---

## 2. Perché non basta il flattening

Idea scartata (ma non del tutto): espandere la compound nei suoi layer
annidati con i transform composti e renderizzare tutto in un solo pass, così
che raggruppare non costi nulla.

Non è sempre valido:

- **Filtri.** Sono per-layer, dentro il fragment shader di ogni layer: non
  esiste uno stadio "dopo l'ultimo layer" su cui applicare i filtri del
  gruppo senza un intermedio rasterizzato (che è la compound clip stessa).
  Applicarli a ciascun layer annidato è corretto solo se il filtro
  *distribuisce* su `over`: vale per le funzioni lineari e pointwise
  (`Grayscale`, l'unico esistente oggi, è una matrice colore), non vale per
  nessun filtro spaziale (`blur(A over B) != blur(A) over blur(B)`) né per
  nessuna non-linearità (gamma, contrasto, saturazione con clamp, curve,
  chroma key). Funzionerebbe per accidente finché c'è un filtro solo.
- **Opacità di gruppo.** Con `opacity < 1` e ≥2 layer annidati sovrapposti:
  rasterizzato sfuma il gruppo (il layer sotto resta coperto), flattened
  sfuma ogni layer per conto suo e **il layer sotto traspare attraverso
  quello sopra**. Immagini diverse, la seconda è quella sbagliata.
- **`Transform` non è chiuso per composizione.** `T_gruppo ∘ T_annidato`
  deve entrare nei campi di `vv_core::Transform` (crop, zoom[2], position,
  rotation, anchor, flip): rotazione esterna ∘ zoom anisotropo interno ∘
  rotazione interna dà una matrice 2x2 generale, non rappresentabile.
  Servirebbe generalizzare transform e shader a una `mat3x3`. In più `crop`
  è in pixel nativi del media del singolo layer, il crop esterno è in pixel
  della timeline annidata: spazi diversi da riconciliare.
- **Clipping al canvas annidato.** Ciò che esce dai bordi della timeline
  annidata oggi sparisce; flattened non c'è più nulla che tagli, e un layer
  spostato oltre il margine diventa visibile nella timeline esterna.
  Servirebbe un rect-clip per layer, derivato dal transform del gruppo.
  Stesso discorso per `fit_factors`.

Resta utilizzabile come ottimizzazione opzionale quando il gruppo
"distribuisce" davvero, ma il guadagno è **un solo render pass**: non vale
i casi limite. Il costo vero è altrove (§1).

---

## 3. Il piano: device condiviso, compound come sotto-pass

### 3.1 `Layer::Texture` nel compositor

Variante di `vv_render::Layer` che prende una `wgpu::Texture` già pronta
invece dei piani YUV. Il bind group layout attuale dichiara
`TextureSampleType::Float { filterable: true }`, D2: una view `Rgba8Unorm`
**lo soddisfa già**. Quindi bind della texture RGBA allo slot 0, placeholder
su 1/2/5, nuovo `Fill::Rgba`, e in `transform.wgsl` un ramo che campiona
come `vec4` e salta `yuv_to_rgb`. Niente seconda pipeline, niente secondo
bind group layout.

`OUTPUT_FORMAT` è `Rgba8Unorm` (non sRGB): nessuna conversione di gamma
implicita nel sample.

### 3.2 Premoltiplicazione — c'è un bug latente da sistemare qui

La pipeline usa `BlendState::ALPHA_BLENDING`: su clear trasparente il
risultato è **premoltiplicato** (`rgb = a·C`, `alpha = a`).
`rgba_to_yuv420_with_alpha` però tratta quel `rgb` come straight, e lo
shader lo rimoltiplica per l'alpha → **α² sui bordi semitrasparenti di una
compound clip**, già oggi. Da confermare con un test.

Nel nuovo path: un-premultiply nello shader (`rgb / max(a, eps)`), che
mantiene una pipeline sola, oppure un secondo blend state
`PREMULTIPLIED_ALPHA_BLENDING` per i soli layer texture.

### 3.3 Il pool di texture ricicla l'output

`render_layers_to_texture_with_clear` fa
`give_back(&mut pool.outputs, [output_texture.clone()])` e restituisce la
stessa texture: **è riciclata**, il render successivo della stessa
dimensione ci disegna sopra. Se qualcuno la conserva → corruzione
silenziosa, non un crash. Serve una variante che non rimetta l'output nel
pool, o una restituzione al pool via `Drop` dell'handle.

### 3.4 Device condiviso col worker

`RenderAhead::spawn` riceve `cc.wgpu_render_state` e costruisce
`Compositor::new(device, queue)` invece di `new_headless()`.
`wgpu::Device`/`Queue` sono `Send + Sync` e già dietro `Arc`. Due
`Compositor` sullo stesso device vanno bene (il `pool` è un `Mutex` per
istanza). Fallback a `new_headless` quando non c'è render state (test,
export).

### 3.5 La compound smette di essere un media cachato

Questa è la parte che semplifica il codice. Una volta che il compositing è
sullo stesso device e costa un pass, non c'è motivo di *materializzare e
cachare* il frame composto:

- il worker torna a fare **solo decode**, anche dentro le timeline annidate
  (la parte di `clipped_media_segments` che cammina nel nesting per tenere
  caldi i media veri resta identica);
- al momento di comporre il frame esterno, una clip compound produce un
  `Layer::Texture` renderizzata al volo in una texture **transitoria dal
  pool**, rilasciata subito dopo. Ricorsivo per il nesting.

Spariscono da `render_ahead.rs`: `compose_compound_segments`,
`MAX_COMPOUND_PASSES`, `CacheOnlyProvider`, `compose_frame_at`, e tutto il
secondo canale `(real, compound)` che oggi attraversa
`collect_media_segments` / `crossing_borrowed_segments` / `walk_and_fill`.
Sparisce la logica "layer non ancora pronto → ritento al giro successivo", e
sparisce l'invalidazione via `content_hash` / `Project::touch_compound` per
i frame composti: non c'è più nulla di stantio da invalidare.

Costo per frame esterno: 1 render pass + 1 texture transitoria per istanza
di compound. A 1080p60 è rumore. **Zero VRAM persistente, zero readback,
zero conversione.**

Perché non cachare le texture composte: 1080p RGBA8 = 8.3 MB/frame, un
lookahead di 3 s a 60 fps sarebbe 1.5 GB di VRAM. (Nota: anche oggi in RAM
sono 5.2 MB/frame → 930 MB, tenuti a bada solo dallo sfratto.)

Contropartita: si ricompone a ogni repaint anche a playhead fermo, e non c'è
riuso quando lo stesso frame compound serve due volte (i due lati di una
crossing transition che ne prende il bordo in prestito). Si mitiga con una
LRU minuscola — una decina di texture, chiave `(media_id, frame locale,
risoluzione)` — non con una cache a orizzonte di lookahead.

### 3.6 Export

`export.rs` compone la compound in sincrono e vuole un `FrameYuv420` perché
`FrameProvider::frame_for` restituisce quello. Generalizzare il tipo di
ritorno (es. `enum ProvidedFrame { Yuv, Texture }`) porterebbe lo stesso
guadagno all'export, che paga identici readback + conversione per frame
compound. È offline: non urgente, va in coda.

---

## 4. Ordine di lavoro

1. `Layer::Texture` + `Fill::Rgba` + un-premultiply nello shader, con test
   di equivalenza contro il path attuale e un test che inchioda l'α² dei
   bordi (§3.2).
2. Variante di `render_layers_to_texture` che non ricicla l'output (§3.3).
3. Device condiviso al worker (§3.4).
4. Compound come sotto-pass in `clip_layer` / `track_layers_at`, e rimozione
   del canale compound da `render_ahead.rs` (§3.5).
5. Export sullo stesso path (§3.6).

---

## 5. Perché non cachare il composito invece dei media

Domanda ricorrente: la cache per media (`SharedFrameCache`, chiave
`(MediaId, frame sorgente)`) fu decisa quando non c'era compositing; le
compound clip cambiano la valutazione? Sarebbe più veloce, o più pulito,
cachare l'ultimo passaggio della pipeline — il frame compositato della
timeline?

**No.** La premessa non regge: le compound non sono lente perché il
compositing sia caro, ma perché *quel* path fa readback bloccante +
conversione su CPU + secondo device (§1). Tolto quello, comporre una
timeline annidata è un render pass.

Motivi per cui la cache per media resta la scelta giusta:

- **Stabilità dell'identità.** Un frame media è invalidato solo dal file che
  cambia o dal toggle proxy. Un frame compositato dipende da tutto lo stato
  del progetto a quel frame (transform, keyframe, opacità, filtri,
  transizioni, ordine/mute/solo delle track), ricorsivamente dalle timeline
  annidate, e dalla risoluzione del viewer (`OutputFrame::scaled` compone
  alla risoluzione del frame decodificato, non della timeline). Un **ripple
  edit** sposta di N l'indice di ogni frame a valle: l'intera cache del
  composito muore, quella dei media non se ne accorge. Sapere *quali* frame
  invalidare è vero dependency tracking; la risposta conservativa è "tutti".
- **Si cachea ciò che è caro ricalcolare e ad accesso non casuale.** Il
  decode è entrambe le cose — seek, GOP, transito: tutta la macchina di
  `render_ahead.rs` (`BEHIND_CHUNK_FRAMES`, soglia di seek adattiva,
  `TRANSIT_SAFETY_CAP_FRAMES`) esiste solo per quello. Il compositing è
  stateless e ad accesso casuale.
- **Deduplicazione.** Un frame media serve più frame di timeline
  (`rate < 1`) e più clip (clip duplicata, stesso media in due punti,
  compound riusata). Il composito non deduplica nulla.
- **Densità.** Composito 1080p = 3.1 MB I420 / 8.3 MB RGBA, frame media =
  3.1 MB: il composito vince solo con molti layer attivi. Su una timeline a
  una track è pari o peggio.
- **L'interazione dominante è l'editing, non il playback.** Una cache il cui
  hit rate collassa a ogni modifica dà il comportamento peggiore quando fa
  più male: sposti un keyframe e ri-decodifichi, invece di ri-comporre da
  cache.

Cosa c'è di vero nell'intuizione:

- Il doppio canale `(real, compound)` in `render_ahead.rs` esiste
  **precisamente perché** la cache è indicizzata per media e una compound
  non è un media. Una cache per frame di timeline farebbe sparire il caso
  speciale — ma §3.5 ottiene la stessa pulizia senza prendersi in carico
  l'invalidazione, perché la compound smette di essere un'entità cachata.
- Argomento diverso e valido: **la varianza sul thread UI**. Oggi tutto il
  compositing avviene dentro il repaint di egui, e il clock del playback non
  può assorbirne i picchi. Un **ring corto** di texture pre-compositate
  (0.25–0.5 s, 15–30 frame, 125–250 MB a 1080p), riempito da un worker e
  buttato per intero a ogni modifica del progetto, ridurrebbe il thread UI a
  un blit. Invalidazione banale: l'orizzonte è così corto che rifarlo costa
  poco. È un **tier 2 sopra** la cache dei media, non al posto suo, ed è
  indipendente dalle compound: da valutare **dopo** §3, misurando se la
  varianza è un problema reale.

Cosa cambierebbe davvero la valutazione: compositing caro per davvero —
stack a molti layer, filtri spaziali pesanti (blur), 4K multi-stream,
effetti con dipendenza temporale. Anche allora la risposta non è
"sostituire" ma un **tier 3**: render cache su disco, opt-in, per sezione,
con chiave un content hash dello stato effettivo delle clip — di cui
`content_hash` / `Project::touch_compound` sono già il germe.

---

## 6. Quick win, se serve un risultato prima del piano

Indipendenti e compatibili col piano:

- conversione RGBA→YUV+alpha **su GPU**, estendendo `rgba_to_i420.wgsl` per
  scrivere anche il piano alpha: toglie il costo dominante (§1.5) e riduce
  il readback da 8.3 a 5.2 MB;
- poolare il buffer di readback del path RGBA come fa già quello i420;
- readback asincrono (submit di N frame, map dopo) invece di bloccare per
  frame;
- `rayon` sul loop di conversione, se resta su CPU;
- compositing delle compound su un thread proprio, per non fermare il
  decode.

---

## 7. Vincoli da non perdere di vista

- **Un pass per layer** (`LoadOp::Load` in un pass separato per ciascuno):
  col nesting i pass si moltiplicano. Indipendente da questo lavoro, ma è il
  prossimo tetto.
- `OutputFrame::scaled`: l'anteprima compone alla risoluzione del frame
  decodificato, non della timeline. La texture della compound va
  renderizzata a quella stessa risoluzione, o si aggiunge uno scaling.
- Submit concorrente sulla stessa queue da UI e worker: wgpu serializza
  internamente, e con §3.5 il worker quasi non tocca più la GPU.
