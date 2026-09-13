//! Frame cache LRU, chiave `(MediaId, SourceFrameIdx)` (milestone 2).
//! TODO: `LruCache<(MediaId, FrameIdx), DecodedFrame>` con budget in RAM
//! configurabile, condivisa tra i worker di decode-ahead e il compositor.
