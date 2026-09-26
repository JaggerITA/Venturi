# MACOS_NATIVE_MENU — menu in the system menu bar on macOS

Status: not started. On macOS the menus currently live in the egui bar inside
the window (`app_menu.rs`, `show_menu_bar`), as on Linux. Goal: a native menu
in the system menu bar on macOS, egui menu unchanged on Linux.

## Approach

eframe/egui has no native menus: use the **`muda`** crate (the one from
Tauri), compatible with winit on macOS, as a
`cfg(target_os = "macos")`-only dependency.

1. **`MenuCommand`**: a single enum of commands (Open, Save, SaveAs, Import,
   ImportOtio, Export, ExportOtio, Settings, Undo, Redo, Copy, Cut, Paste,
   PasteAttributes, Delete, RippleDelete, Split, ZoomIn, ZoomOut, Fullscreen,
   About, OpenRecent(path), JumpHistory(pos), checkbox toggles…) with a
   single `dispatch`. Both the egui menu and the native one produce commands,
   the logic is not duplicated. A useful refactor on its own.
2. **Native menu** (`macos_menu.rs`): built once after eframe starts (in the
   app creation closure, when NSApp exists). Events read every frame from
   `MenuEvent::receiver()`; `MenuEvent::set_event_handler` calls
   `ctx.request_repaint()`, otherwise with an idle UI the click stays queued.
3. **State synced every frame** (cheap): `set_enabled` on conditional items
   (Export, Copy/Cut with a selection, Paste with a clipboard…),
   `set_checked` on the `CheckMenuItem`s (Properties, Audiometer, Scrub audio,
   Selection follows playhead, Use proxy).
4. **Dynamic submenus** (recent projects, undo history): rebuilt when they
   change. The whole menu is rebuilt on a language change.
5. **Standard application menu**: "Venturi" with About, Settings (⌘,),
   Hide/Hide Others, Quit (`PredefinedMenuItem`).

## Obstacles

- **Non-native widgets in Playback → Proxy**: the read-ahead, read-behind
  and video cache `DragValue`s cannot live in a native menu. Separate
  preliminary step: move them to the Settings window (sensible on Linux
  too). Same for the "zoom hint" label of the Timeline menu.
- **Accelerators**: with an accelerator on the item, macOS intercepts the
  key before winit, which no longer sees it. Therefore:
  - the shortcut must be handled only once (menu event, not
    `handle_shortcuts`), or the items stay without an accelerator and show
    the shortcut in the text only;
  - shortcuts are configurable: accelerators must be updated on a keymap
    change, with an `egui::Key` → `muda` mapping (`<`/`IntlBackslash` by
    hand, see the ISO fix in `fix_iso_key`);
  - ⌘C/⌘V/⌘X in the Edit menu risk breaking copy/paste in egui text
    fields: no accelerator, or forwarded to egui as
    `Event::Copy`/`Event::Paste`.
- **Testing**: no Mac in development, every iteration goes through CI + a
  real Mac.

## Estimate

400-600 lines, mostly a reorganisation of `app_menu.rs`. Order: proxy
settings in the Settings window → `MenuCommand` (Linux only, testable) →
native macOS menu.
