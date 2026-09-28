"""Transcribe one audio stream of a media file with word timestamps.

Writes <input>.transcript.json next to the input (or --out) and prints
one line per segment.
"""
import argparse
import json
import subprocess
import tempfile
import wave

import ctranslate2
import numpy as np
from faster_whisper import WhisperModel

MODELS = {"crisper": "nyrahealth/faster_CrisperWhisper"}

p = argparse.ArgumentParser()
p.add_argument("input", help="media file, relative to /work")
p.add_argument("--stream", type=int, default=0, help="audio stream index")
p.add_argument("--model", default="crisper",
               help="crisper (verbatim, keeps retakes) or a Whisper model such as medium")
p.add_argument("--language", default=None, help="e.g. it; autodetect if unset")
p.add_argument("--pause-db", type=float, default=-45.0,
               help="RMS below this (dBFS) counts as a pause")
p.add_argument("--min-pause", type=float, default=0.3, help="seconds")
p.add_argument("--chunk", type=float, default=10.0,
               help="transcribe pieces of about this many seconds, split at pauses; 0 = whole file")
p.add_argument("--device", choices=["auto", "cpu", "cuda"], default="auto")
p.add_argument("--out", default=None)
args = p.parse_args()


RATE = 16000
PAD = 0.2


def pauses(audio, threshold_db, min_len):
    hop = RATE // 100
    frames = audio[: len(audio) // hop * hop].reshape(-1, hop)
    db = 20 * np.log10(np.sqrt((frames ** 2).mean(axis=1)) + 1e-9)
    quiet = np.concatenate(([False], db < threshold_db, [False]))
    edges = np.flatnonzero(np.diff(quiet.astype(np.int8)))
    return [{"s": s / 100, "e": e / 100}
            for s, e in zip(edges[::2], edges[1::2]) if (e - s) / 100 >= min_len]


def chunks(duration, quiet, target):
    """(start, end) pieces of speech, each ending inside a pause."""
    if target <= 0:
        return [(0.0, duration)]
    out, start = [], 0.0
    for q in quiet:
        if q["s"] - start >= target:
            out.append((max(start - PAD, 0.0), q["s"] + PAD))
            start = q["e"]
    # Whisper makes up words ("Grazie.") in trailing silence.
    end = quiet[-1]["s"] + PAD if quiet and quiet[-1]["e"] >= duration - 0.01 else duration
    if start < end:
        out.append((max(start - PAD, 0.0), end))
    return [c for c in out if c[1] - c[0] > 2 * PAD]


def looped(segments, run=5):
    words = [w.word.strip(" ,.'").lower() for s in segments for w in s.words]
    return any(len(set(words[i:i + run])) == 1 for i in range(len(words) - run + 1))


with tempfile.NamedTemporaryFile(suffix=".wav") as wav:
    # Decoding via ffmpeg because faster-whisper always takes the first audio stream.
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", args.input,
         "-map", f"0:a:{args.stream}", "-ac", "1", "-ar", "16000", wav.name],
        check=True,
    )
    device = args.device
    if device == "auto":
        device = "cuda" if ctranslate2.get_cuda_device_count() else "cpu"
    model = WhisperModel(MODELS.get(args.model, args.model), device=device,
                         compute_type="float16" if device == "cuda" else "int8",
                         download_root="/models")
    with wave.open(wav.name) as w:
        audio = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)
    audio = audio.astype(np.float32) / 32768
    quiet = pauses(audio, args.pause_db, args.min_pause)
    out = {"language": args.language, "segments": [],
           # Word timestamps drift by a few tenths (more with crisper): cut inside these.
           "pauses": quiet}
    # Decoding 30 s windows, each conditioned on the previous text, merges retakes
    # into one sentence, loops or drops whole passages; short independent pieces don't.
    for c0, c1 in chunks(len(audio) / RATE, quiet, args.chunk):
        # Temperature fallback samples, so a looping piece usually comes out right on retry.
        for _ in range(3):
            segments, info = model.transcribe(audio[int(c0 * RATE):int(c1 * RATE)],
                                              language=out["language"],
                                              word_timestamps=True, beam_size=5,
                                              condition_on_previous_text=False)
            segments = list(segments)
            if not looped(segments):
                break
        out["language"] = info.language
        for s in segments:
            out["segments"].append({
                "start": c0 + s.start, "end": c0 + s.end, "text": s.text,
                "words": [{"s": c0 + w.start, "e": c0 + w.end, "w": w.word}
                          for w in s.words],
            })
            print(f"[{c0 + s.start:7.2f}-{c0 + s.end:7.2f}] {s.text}", flush=True)

out_path = args.out or f"{args.input}.transcript.json"
with open(out_path, "w") as f:
    json.dump(out, f, ensure_ascii=False, indent=1)
print(f"-> {out_path}")
