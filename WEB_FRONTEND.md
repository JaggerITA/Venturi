# vibevideo — frontend web/tablet: indicazioni per l'implementazione

Documento di design per disaccoppiare l'editing da egui e permettere un
client web (es. tablet in rete locale) che pilota un backend headless in
esecuzione su un PC fisso. Scritto per essere usato come punto di partenza
in una chat/sessione separata — non tocca il lavoro sul frontend nativo
egui già in corso, che resta il client "principale" fino a nuovo avviso.

Nasce da una domanda esplorativa dell'utente ("e se il frontend fosse
web-based? posso montarlo su un tablet collegato al backend su un PC
fisso?"), non da un requisito già deciso: prima di implementare, verificare
che le scelte di stadio 1 (vedi sotto) siano ancora quelle desiderate.

## Perché è fattibile senza riscrivere il backend

Il progetto è già diviso in crate data-oriented, e solo `vv-app` è legato a
egui:

| Crate | Cosa fa | Riusabile as-is? |
|---|---|---|
| `vv-core` | modello dati (`Project`/`Timeline`/`Clip`), command pattern, `History` | sì, al 100% |
| `vv-media` | probe/decode ffmpeg, `DecodeAhead` | sì, al 100% |
| `vv-render` | compositor GPU headless (wgpu), generatori (solid color) | sì, al 100% |
| `vv-audio` | mixer delle track audio, output via cpal | sì per il monitoring locale sul PC (vedi § Audio) |
| `vv-app` | UI egui/eframe, `TimelineAudio`, orchestrazione (`main.rs`), disegno timeline (`timeline_ui.rs`) | **solo in parte**: `TimelineAudio`, `History`, `drive_playback`, `ensure_active_clip_matches_playhead`, `sync_selection_to_playhead` sono Rust puro non legato a egui e si spostano quasi invariati in un nuovo host; il disegno (`timeline_ui.rs`) e i pannelli egui in `main.rs` no, si riscrivono nel client web |

Conclusione: non serve un rewrite del backend. Serve un nuovo processo che
sostituisce il loop di eventi di eframe con un server di rete, più un
client web che rifà solo la parte di presentazione.

## Cosa NON cambia

- Decode, compositing GPU, encode restano sul PC. Il tablet non decodifica
  né compone nulla di suo (a parte, eventualmente, il video H.264 già
  pronto per la preview — vedi § Canale di anteprima).
- Il modello dati e il command pattern restano quelli di `vv-core`: è
  esattamente il motivo per cui questo passaggio è fattibile a basso
  rischio. Ogni `Command` esistente (`InsertClip`, `SplitClip`,
  `RippleDeleteAllTracks`, `MoveClips`, `LinkClips`, `CompositeCommand`,
  ...) diventa un messaggio RPC quasi sena modifiche.

## Nuovo componente: `vv-server`

Nuovo crate, analogo a `vv-app` ma senza UI: tokio + axum (o `tungstenite`
puro se si vuole restare minimi), possiede `Project` + `History` come
unica fonte di verità, e sostituisce il loop `App::ui()` di eframe con un
loop pilotato da messaggi di rete. Tiene in vita `TimelineAudio`/`drive_playback`
esattamente come oggi (a interval fisso, o meglio ancora guidato da un
timer dedicato invece che dal ciclo di ridisegno di egui).

Punti da portare via da `vv-app/src/main.rs` pressoché invariati (sono già
Rust puro, testato con gli unit test esistenti):
`TimelineAudio`, `History`, `group_members`,
`ensure_active_clip_matches_playhead`, `drive_playback`,
`sync_selection_to_playhead`, `split_at_playhead`, `delete_selected`,
`ripple_delete_selected`, `add_media_to_timeline`.

## Canale di controllo (comandi + stato)

WebSocket, un solo endpoint (es. `/ws`), messaggi JSON per iniziare
(bincode è un'ottimizzazione successiva, non necessaria finché il
progetto resta su LAN con pochissimi client).

**Client → server**: un comando applicativo, che ricalca 1:1 i
`Command` di `vv-core`:

```json
{ "type": "SplitClip", "track_index": 0, "clip_id": 5, "split_at": 1200 }
{ "type": "RippleDeleteSelected" }
{ "type": "SetSelection", "track_index": 0, "clip_id": 5 }
{ "type": "SetPlayhead", "frame": 1200 }
{ "type": "Undo" }
{ "type": "TogglePlayback" }
```

Sul lato server, ogni messaggio si traduce in una chiamata diretta ai
metodi già esistenti su `VibeVideoApp`-equivalente (`split_at_playhead`,
`ripple_delete_selected`, ecc.) o in un `History::do_command` con il
`Command` di `vv-core` corrispondente costruito dai campi del messaggio.

**Server → client**: dopo ogni comando applicato, broadcast dello stato
aggiornato a tutti i client connessi. Per iniziare, niente diffing:
`Project` (clip, non pixel) è piccolo, un editing "reale" (poche centinaia
di clip) resta ben sotto i 100KB serializzato — semplicità sul
performance banale in questo range. Aggiungere diffing solo se/quando
diventa un collo di bottiglia misurato, non prima.

```json
{
  "type": "ProjectState",
  "project": { "...": "..." },
  "selected": [0, 5],
  "playhead": 1200,
  "playing": false
}
```

### Multi-client e concorrenza

Se più tablet/browser si connettono insieme, decidere subito una regola
semplice per evitare comandi in conflitto: **il server è autoritativo e
seriale** (un solo `History` condiviso, i comandi si applicano in ordine
di arrivo, non serve locking applicativo lato client). Non serve subito un
concetto di "ownership"/turni: dato l'uso previsto (un editor alla volta
che passa dal PC al tablet, non due persone che editano insieme), un
last-write-wins sull'ordine di arrivo dei messaggi è sufficiente. Se in
futuro serve editing collaborativo vero, è un progetto a parte (CRDT/OT),
non uno scope di questo documento.

## Canale di anteprima video

La parte davvero nuova e delicata, da costruire in due stadi separati.

### Stadio A — richiesta puntuale (scrub, pausa)

Endpoint HTTP `GET /frame?playhead=1200` che il backend risolve
esattamente come fa oggi il viewer interno (compositing del frame corrente
via `vv-render::Compositor`, eventuale generatore SolidColor) e restituisce
come singolo JPEG. Il client web lo mostra in un semplice `<img>`. Nessun
concetto di streaming: un frame per richiesta, la stessa identica pipeline
di composizione che gira già in `vv-app` oggi, solo esposta via HTTP invece
che disegnata su una `egui::TextureHandle`.

Questo stadio da solo copre già "arrangiare, tagliare, spostare clip dal
tablet" con feedback visivo per lo scrub — è il più alto rapporto
valore/sforzo e va fatto per primo.

### Stadio B — playback live

Durante la riproduzione, streaming continuo dei frame compositati sul
canale WebSocket (binario, non lo stesso canale JSON dei comandi — separare
i due per non far accodare i controlli dietro ai frame). Partire dalla
soluzione più semplice possibile:

- **MVP**: un frame JPEG per messaggio binario, a framerate della
  timeline o a un framerate ridotto se serve contenere banda/CPU (es. 15fps
  invece di 25-30 in preview, non nell'export finale). Su rete locale la
  banda non è il vincolo (1080p JPEG ≈ 100-300KB/frame → 3-9MB/s a 30fps,
  ampiamente dentro una LAN/WiFi decente); il vincolo vero è la latenza di
  encode+rete+decode, da misurare prima di ottimizzare oltre.
- **Se il MVP non basta** (banda o CPU di encode JPEG sul PC diventano un
  problema reale, misurato): passare a un codec compresso vero (H.264)
  incapsulato in piccoli frame WebSocket, decodificato lato browser con
  `WebCodecs` (hardware-accelerated, supportato su Chrome/Safari moderni,
  quindi anche su iPad). Evitare WebRTC salvo necessità dimostrata: su LAN
  a bassa latenza non serve la sua complessità (SDP/ICE/negoziazione),
  serve solo se in futuro si vuole accedere da fuori la rete locale.

## Audio

Per la prima versione: **l'audio resta locale al PC**, non viene
streammato al tablet. Motivazioni: evita un intero problema di
sincronizzazione audio/video sulla rete (jitter, buffering separato dal
video), e nell'uso previsto (PC fisso nella stanza) chi lavora sente
comunque l'audio dagli altoparlanti/cuffie del PC mentre guarda/controlla
dal tablet. Da rivedere solo se l'uso reale mostra che serve davvero
sentire dal tablet (es. PC in un'altra stanza) — a quel punto: Web Audio
API + PCM o Opus sullo stesso canale binario del video, con lo stesso
discorso su complessità di WebCodecs vs MVP grezzo.

## Piano a stadi (ordine consigliato)

1. `vv-server`: canale di controllo (comandi + stato) via WebSocket, più
   endpoint HTTP per il frame singolo su richiesta (Stadio A sopra). Client
   web minimale: disegna la timeline da `ProjectState` (canvas o SVG),
   invia comandi, mostra il frame corrente come `<img>` che si aggiorna a
   ogni scrub/selezione/comando. Risultato: si può tagliare, spostare,
   fare ripple-delete dal tablet, con feedback visivo statico.
2. Streaming di playback live (Stadio B, MVP JPEG-over-WS). Risultato: il
   tablet può anche premere play e vedere la riproduzione, non solo
   editare da fermo.
3. (Solo se necessario, misurato) ottimizzazione dello streaming
   (WebCodecs/H.264) e/o audio verso il tablet.

Non anticipare lo stadio 3 senza aver verificato che lo stadio 2 sia
davvero insufficiente nell'uso reale: è dove la complessità sale più
rapidamente del valore aggiunto.

## Decisioni ancora aperte (da chiudere nella chat separata)

- Framework del client web: vanilla TS + canvas è il più vicino allo stile
  "niente overhead superfluo" del resto del progetto; React/Svelte vanno
  bene se si preferisce velocità di sviluppo dell'interfaccia a costo di
  qualche dipendenza in più. Nessuna delle due cambia il ragionamento sopra.
- Libreria server: `axum` (più comune, buon supporto WebSocket) vs
  `tungstenite` diretto su `tokio` (più minimale). Consigliato `axum` per
  meno codice di infrastruttura da mantenere a mano.
- Nome/percorso del binario `vv-server` e se debba condividere lo stesso
  workspace Cargo di `vv-app` (raccomandato: sì, stesso workspace, nuovo
  membro `crates/vv-server`, così riusa `vv-core`/`vv-media`/`vv-render`
  come dipendenze di path senza duplicazione).
- Se e quando servirà autenticazione minima (anche solo un token statico
  in query string) prima di esporre il WebSocket oltre `localhost` — non
  necessario finché resta bind su un'interfaccia LAN fidata, ma da
  decidere esplicitamente prima di aprire la porta oltre `127.0.0.1`.

## Testing

Coerente con l'approccio già in uso nel resto del progetto (funzioni pure
estratte apposta per essere testabili, niente mock del decoder/encoder):
la logica di traduzione messaggio→`Command` si presta a test puri senza
rete (dato un messaggio JSON, verificare quale `Command`/metodo viene
invocato); il ciclo WebSocket vero e proprio si testa con un client di
test in-process (es. `tokio-tungstenite` contro un server avviato su una
porta locale nel test), seguendo lo stesso principio "vero I/O, non mock"
già usato per ffmpeg nei test di `vv-media`.
