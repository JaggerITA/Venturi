# TRANSCRIPTION — speech-to-text through a Whisper-compatible API

**Status:** proposed, to discuss. Not started.

## Why

In the first real MCP session (2026-09-27, "remove the duplicated takes")
the agent had to install faster-whisper in a throwaway venv, transcribe the
original file, convert seconds to media frames by hand, then map those
frames onto a timeline already shortened by an earlier ripple cut. It
worked, but the setup is repeated every session, and the manual mapping is
the step where cuts go wrong (some duplicated takes were left in).

Transcription through an external API gives:
1. an MCP `get_transcript(media_id)` with times already in media frames,
   without adding a C++ dependency to Venturi;
2. nothing for users who do not configure it: the tool says so, and the
   agent falls back to its own tools;
3. the base for script-based editing (cutting by selecting text) and for
   automatic subtitles, both separate features built on top of this one.

## The API

The de facto standard `POST /v1/audio/transcriptions` (multipart: `file`,
`model`, `language?`, `response_format=verbose_json`,
`timestamp_granularities[]=word|segment`). It is served by OpenAI, Groq and
by local servers (whisper.cpp `server`, speaches/faster-whisper-server,
LocalAI), so one client covers both cloud and fully offline setups.

Known differences to handle:
- **Word timestamps** exist on OpenAI `whisper-1`, Groq and most local
  servers. They do not exist, as far as known, on `gpt-4o-transcribe`.
  Words are returned when the service gives them; otherwise only segments.
- **Request size**: OpenAI caps uploads at 25 MB. Audio is always sent as
  mono 16 kHz, compressed (Opus or MP3, about 1 MB per 10 minutes), and long
  media are split into chunks (e.g. 10 min, with a couple of seconds of
  overlap, cut at a quiet point where possible). Timestamps are shifted by
  each chunk's offset; words in the overlap are deduplicated.
- **Errors**: 401 (bad key), 413 (too large), 429 (rate limit: retry with
  backoff), network down. Each one gives a plain message in the UI and in
  the MCP result.

## Design

### Settings (Settings > Integrations)

- Service: *None* (default) / *OpenAI* / *Custom (OpenAI-compatible)*.
  The presets only fill in the base URL and a suggested model.
- Base URL, model, API key. The key can also come from an environment
  variable (`VENTURI_TRANSCRIPTION_API_KEY`), which takes precedence, so it
  does not have to sit in plain text in `settings.json`.
- Language: *Automatic* or a fixed one (see open points).
- An explicit notice when the URL is not local: the audio of the media is
  sent to that service.

### Core (`vv-session`, UI-free)

- `vv_session::transcription`: extract a stream to mono 16 kHz (the same
  decode path as the mixer, then encode), chunk, send, merge, and convert
  seconds to **media frames** with the media's fps, using the mixer's
  convention (sample 0 = start of the media).
- A session job like import and export: `Session::transcribe(media_id,
  stream) -> JobId`, progress by chunks, `SessionEvent::TranscriptReady` /
  `TranscriptFailed`, cancellable. The HTTP client runs on the job's thread
  (a small blocking client such as `ureq`, no async runtime in vv-session).
- The result:

  ```rust
  pub struct Transcript {
      pub language: String,
      pub segments: Vec<Segment>,   // text, start/end in media frames
      pub words: Option<Vec<Word>>, // text, start/end in media frames, probability?
      pub service: String,          // model and URL, for the record
  }
  ```

- **Cache** by `(content_hash, stream, language)` in
  `$XDG_CACHE_HOME/venturi/transcripts/` (same scheme as waveforms and
  proxies): each file is transcribed once, whatever project it is in, and a
  relink to the same content keeps it.

### MCP (`vv-mcp`)

A transcription can take minutes and MCP clients time out on long calls, so
it follows the export pattern:
- `transcribe(media_id, stream?, language?)` returns the cached transcript
  at once if there is one, or starts a job and returns `{job_id}`.
- `get_transcript(media_id, stream?)` returns the transcript, its progress,
  or "not transcribed yet". With `words` when available. Times in media
  frames, plus seconds for readability.
- Not configured: a clear error ("transcription is not set up in Venturi:
  Settings > Integrations, or transcribe the file with your own tools").
- In attach mode the job shows in the UI like an import.

The agent still has to map media frames onto the timeline after earlier
cuts. See the separate item below.

### UI (minimal, in this plan)

- Media pool context menu: *Transcribe*, with progress, and a mark on media
  that have a transcript.
- *Export transcript as SRT* from the same menu, straight from the
  segments (times in the media's own timing).

Subtitles on the timeline and script-based editing are **not** part of
this plan.

## Related, independent: deleting by media ranges

With a transcript in media frames, an agent's hardest step is still
turning "source frames 3390..3648 of this media" into timeline ranges once
the timeline has already been cut. Proposed separately (it helps any
transcript source, including the agent's own whisper):
`delete_ranges(timeline_id, media_id, media_ranges, ripple)`, where Venturi
finds where those source frames ended up on the timeline (every clip of that
media, every track, after any earlier cuts) and removes them. Worth doing
before or together with this plan.

## Steps (draft)

| # | Step |
|---|------|
| 1 | Audio extraction + chunking to a small mono file (vv-media/vv-session), unit tests with generated audio |
| 2 | HTTP client for `/v1/audio/transcriptions`, merge of chunks, seconds → media frames; tests against a local mock server |
| 3 | Session job + cache + events |
| 4 | Settings (service, URL, model, key/env var, language) + notices, en/it |
| 5 | MCP `transcribe` / `get_transcript` |
| 6 | Media pool: Transcribe, progress, SRT export |
| 7 | Docs (`docs/MCP.md`, a transcription section) |

Manual test: a local whisper.cpp or speaches server in the podman
container, so no key is needed.

## Open points

1. **Default service**: *None* (the user chooses) with two presets
   (OpenAI, custom URL)? Proposed: yes.
2. **Where the transcript lives**: only in the cache (regenerable, stays
   out of the save format while it is still unstable), or also in the
   project (travels with it, natural for script-based editing)? Proposed:
   cache now, decide with the script-editing plan.
3. **Word timestamps**: required (fewer usable services) or "when
   available"? Proposed: when available, and the tool says which it is.
4. **Language**: automatic, per project, or per media? Proposed: automatic
   by default, overridable per call (MCP) and in the Transcribe dialog.
5. **Speaker labels (diarization)**: not in the standard endpoint; out of
   scope unless a target service offers it.
6. **Timeline transcript** (the mix, not one media): useful for checking the
   result after cuts. It can be derived from the media transcripts and the
   clips' source ranges without sending audio again; later.
