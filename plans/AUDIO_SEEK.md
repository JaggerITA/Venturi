# AUDIO_SEEK — seek before decoding the audio a range mix reads

Follow-up to a004666 (Vikunja #22). A range mix (`export::mix_audio_track`,
also behind MCP `get_audio_levels` on a timeline) keeps only the frames it
reads, but still decodes every file from its start: a few seconds near the
end of a one-hour file cost the decode of the whole hour.

**Status:** proposed, not started.

---

## 1. Idea

When a window starts far into the file, seek to a little before it, decode
from there, and throw away what comes before the window:

1. Window start below a threshold (e.g. 30 s): decode from the start, as
   today.
2. Otherwise seek backward on the audio stream to `window start - preroll`
   (preroll e.g. 3 s).
3. The index of the first decoded sample comes from the frame's pts:
   `(pts - stream start_time) * out_rate`. From there samples are counted
   as today and those before the window are dropped.
4. The preroll lets the decoder rebuild its state (MDCT overlap, MP3 bit
   reservoir, Opus's 80 ms) and fills swresample's filter.

## 2. Where

`vv-media/src/audio.rs`: a variant of `decode_audio_streams_streaming` that
takes the output sample to start from (per file: the earliest window of all
its streams) and reports the output index of the first chunk it delivers.
`export::decode_windows` passes the earliest window start and offsets its
`decoded` counters by the reported index.

## 3. When the pts can be trusted

Only then, otherwise fall back to the linear decode:

- Containers with an index: MP4/MOV, Matroska/WebM.
- PCM (WAV, AIFF): the position follows from the byte offset, exact.
- **Not** VBR MP3 without a TOC, raw ADTS AAC, and similar: their seek
  estimates the position from the bitrate and can be tens of ms off.

Decided from `ictx.format().name()` (allow-list), plus a sanity check:
missing pts, or a first pts past the target, falls back too.

## 4. What stops being exact

- swresample restarted at an arbitrary input sample places its output grid
  a fraction of a sample off the preview's (< 21 µs at 48 kHz), with tiny
  numeric differences. Inaudible. Exact alignment would mean restarting on
  the conversion grid (at 44.1 → 48 kHz a multiple of 147 input samples)
  and accounting for the resampler delay: not worth it.
- Lossy codecs converge after the preroll, but not always bit for bit
  (Opus).

The range mix is then equal to the whole mix within a tolerance, not
sample-identical. The preview is unaffected (it keeps decoding whole
files).

## 5. Tests

- vv-media: seek-decode of a window of a WAV and of an AAC-in-MP4 against
  the same frames of a whole decode: same length, max difference below a
  small tolerance, and aligned (the cross-correlation peaks at lag 0).
- vv-media: a format outside the allow-list decodes linearly (same output
  as today, exactly).
- vv-session: `mix_audio_track_of_a_range_is_the_same_part_of_the_whole_mix`
  becomes a tolerance comparison, with a window past the threshold so the
  seek actually runs.

## 6. Open points

- Threshold and preroll values: measure the decode speed of a long AAC and
  MP3 first, to see what the seek actually saves.
- Whether the editor could instead reuse the whole-file buffers the preview
  already keeps (`mix_buffers`) for `get_audio_levels`, when they are there.
