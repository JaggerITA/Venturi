"""Transcribe one audio stream of a media file with word timestamps.

Writes <input>.transcript.json next to the input (or --out) and prints
one line per segment.
"""
import argparse
import json
import subprocess
import tempfile

from faster_whisper import WhisperModel

p = argparse.ArgumentParser()
p.add_argument("input", help="media file, relative to /work")
p.add_argument("--stream", type=int, default=0, help="audio stream index")
p.add_argument("--model", default="medium")
p.add_argument("--language", default=None, help="e.g. it; autodetect if unset")
p.add_argument("--out", default=None)
args = p.parse_args()

with tempfile.NamedTemporaryFile(suffix=".wav") as wav:
    # Decoding via ffmpeg because faster-whisper always takes the first audio stream.
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", args.input,
         "-map", f"0:a:{args.stream}", "-ac", "1", "-ar", "16000", wav.name],
        check=True,
    )
    model = WhisperModel(args.model, device="cpu", compute_type="int8",
                         download_root="/models")
    segments, info = model.transcribe(wav.name, language=args.language,
                                      word_timestamps=True, beam_size=5)
    out = {"language": info.language, "segments": []}
    for s in segments:
        out["segments"].append({
            "start": s.start, "end": s.end, "text": s.text,
            "words": [{"s": w.start, "e": w.end, "w": w.word} for w in s.words],
        })
        print(f"[{s.start:7.2f}-{s.end:7.2f}] {s.text}", flush=True)

out_path = args.out or f"{args.input}.transcript.json"
with open(out_path, "w") as f:
    json.dump(out, f, ensure_ascii=False, indent=1)
print(f"-> {out_path}")
