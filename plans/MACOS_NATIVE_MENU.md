# MACOS_NATIVE_MENU — menu nella barra di sistema su macOS

Stato: non iniziato. Su macOS i menu stanno oggi nella barra egui dentro la
finestra (`app_menu.rs`, `show_menu_bar`), come su Linux. Obiettivo: menu
nativo nella barra di sistema su macOS, menu egui invariato su Linux.

## Approccio

eframe/egui non ha menu nativi: si usa il crate **`muda`** (quello di Tauri),
compatibile con winit su macOS, come dipendenza solo
`cfg(target_os = "macos")`.

1. **`MenuCommand`**: enum unico dei comandi (Open, Save, SaveAs, Import,
   ImportOtio, Export, ExportOtio, Settings, Undo, Redo, Copy, Cut, Paste,
   PasteAttributes, Delete, RippleDelete, Split, ZoomIn, ZoomOut, Fullscreen,
   About, OpenRecent(path), JumpHistory(pos), toggle delle checkbox…) con un
   solo `dispatch`. Il menu egui e quello nativo producono comandi, la logica
   non si duplica. Refactor utile anche a sé.
2. **Menu nativo** (`macos_menu.rs`): costruito una volta dopo l'avvio di
   eframe (nella closure di creazione dell'app, quando NSApp esiste). Eventi
   letti ogni frame da `MenuEvent::receiver()`; `MenuEvent::set_event_handler`
   chiama `ctx.request_repaint()`, altrimenti a UI ferma il click resta in coda.
3. **Stato sincronizzato ogni frame** (costa poco): `set_enabled` sulle voci
   condizionali (Export, Copy/Cut con selezione, Paste con clipboard…),
   `set_checked` sulle `CheckMenuItem` (Inspector, Audiometer, Scrub audio,
   Selection follows playhead, Use proxy).
4. **Sottomenu dinamici** (progetti recenti, cronologia undo): ricostruiti
   quando cambiano. Tutto il menu ricostruito al cambio lingua.
5. **Menu applicazione standard**: "Venturi" con About, Impostazioni (⌘,),
   Nascondi/Nascondi altre, Esci (`PredefinedMenuItem`).

## Ostacoli

- **Widget non nativi in Playback → Proxy**: i `DragValue` di read-ahead,
  read-behind e cache video non possono stare in un menu nativo. Passo
  preliminare separato: spostarli nella finestra Impostazioni (sensato anche
  su Linux). Idem la label "zoom hint" del menu Timeline.
- **Acceleratori**: con un acceleratore sulla voce, macOS intercetta il tasto
  prima di winit, che non lo vede più. Quindi:
  - la scorciatoia va gestita una volta sola (evento di menu, non
    `handle_shortcuts`), oppure le voci restano senza acceleratore e mostrano
    la scorciatoia solo nel testo;
  - le scorciatoie sono configurabili: acceleratori da aggiornare al cambio
    di keymap, con mappatura `egui::Key` → `muda` (`<`/`IntlBackslash` a mano,
    vedi il fix ISO in `fix_iso_key`);
  - ⌘C/⌘V/⌘X nel menu Modifica rischiano di rompere copia/incolla nei campi
    di testo egui: senza acceleratore, oppure inoltrati a egui come
    `Event::Copy`/`Event::Paste`.
- **Test**: niente Mac in sviluppo, ogni iterazione passa da CI + Mac reale.

## Stima

400-600 righe, in gran parte riorganizzazione di `app_menu.rs`. Ordine:
impostazioni proxy nella finestra Impostazioni → `MenuCommand` (solo Linux,
testabile) → menu nativo macOS.
