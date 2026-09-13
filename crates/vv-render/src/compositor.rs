//! Pipeline di compositing per-frame (vedi ARCHITECTURE.md § Compositing GPU):
//! upload texture YUV -> shader crop/zoom -> overlay testo -> blend over
//! per track (bottom -> top) -> presenta nella texture del viewer egui.
//! TODO: milestone 5.
