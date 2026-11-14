"""Vocal melody extraction: target notes + F0 contour from the vocal stem.

Produces ``{file_hash}_melody.json`` in the original (unshifted) timeline:

    {
      "version": 1,
      "duration": 210.4,
      "f0": [[t, hz], ...],            # voiced contour, ~25 Hz resolution
      "notes": [{"start": s, "end": e, "midi": 69, "hz": 440.0}, ...]
    }

Notes are segment boundaries from a median-smoothed pYIN F0 track; the
contour is the faint reference line drawn behind them.  The client maps
both onto the active key/tempo variant (time / tempo, midi + key offset),
so only the original-timeline file is ever written here.
"""

import json
import math
import os
import tempfile

FMIN_HZ = 65.0  # C2 — bottom of most singing ranges
FMAX_HZ = 1100.0
FRAME_LENGTH = 2048
HOP_LENGTH = 512
SR = 22050
VOICED_PROB_MIN = 0.4
MIN_NOTE_SEC = 0.12
JUMP_SEMITONES = 3.5
CONTOUR_STRIDE = 2  # ~21 Hz contour resolution is plenty for a faint line


def _hz_to_midi(hz):
    import numpy as np

    return 69.0 + 12.0 * np.log2(np.asarray(hz, dtype=float) / 440.0)


def _median_window(values, window):
    out = list(values)
    half = window // 2
    for i in range(len(out)):
        lo = max(0, i - half)
        hi = min(len(out), i + half + 1)
        chunk = sorted(values[lo:hi])
        mid = len(chunk) // 2
        out[i] = chunk[mid] if len(chunk) % 2 else (chunk[mid - 1] + chunk[mid]) / 2.0
    return out


def _segment_notes(times, midi):
    """Split the voiced midi contour into notes at unvoiced gaps and jumps."""
    notes = []
    i = 0
    n = len(times)
    while i < n:
        if math.isnan(midi[i]):
            i += 1
            continue
        start = i
        prev = midi[i]
        end = i
        while end + 1 < n:
            j = end + 1
            if math.isnan(midi[j]) or abs(midi[j] - prev) > JUMP_SEMITONES:
                break
            prev = midi[j]
            end = j
        notes.append((times[start], times[end], midi[start : end + 1]))
        i = end + 1

    # Drop flicker: notes shorter than MIN_NOTE_SEC are usually detector noise.
    step = (times[1] - times[0]) if n > 1 else 0.05
    min_frames = max(1, int(MIN_NOTE_SEC / max(step, 1e-6)))
    kept = []
    for start, end, values in notes:
        if len(values) < min_frames:
            continue
        midi_value = float(sorted(values)[len(values) // 2])
        kept.append((start, end, midi_value))
    return kept


def extract_melody(vocals_path, output_dir, file_hash):
    """Extract melody from a vocal stem. Returns the melody path or None."""
    melody_path = os.path.join(output_dir, f"{file_hash}_melody.json")
    if os.path.isfile(melody_path):
        return melody_path

    try:
        import librosa
        import numpy as np

        y, sr = librosa.load(vocals_path, sr=SR, mono=True)
        if y is None or len(y) < FRAME_LENGTH:
            print("[nightingale:LOG] Melody skipped: vocal stem too short", flush=True)
            return None

        f0, voiced_flag, voiced_prob = librosa.pyin(
            y, sr=sr, frame_length=FRAME_LENGTH, hop_length=HOP_LENGTH,
            fmin=FMIN_HZ, fmax=FMAX_HZ,
        )
        times = np.arange(len(f0)) * HOP_LENGTH / float(sr)
        voiced = (f0 > 0) & (voiced_flag > 0.5) & (voiced_prob >= VOICED_PROB_MIN)

        midi = np.full(len(f0), float("nan"))
        midi[voiced] = _hz_to_midi(f0[voiced])
        # Median-smooth the contour so vibrato doesn't split notes.
        smooth = _median_window(list(midi), 5)
        midi_s = np.array(smooth, dtype=float)

        kept = _segment_notes(times, midi_s)
        if not kept:
            print("[nightingale:LOG] Melody skipped: no voiced notes found", flush=True)
            return None

        notes = []
        for start, end, value in kept:
            note_midi = int(round(value))
            notes.append({
                "start": round(float(start), 3),
                "end": round(float(end), 3),
                "midi": note_midi,
                "hz": round(440.0 * (2.0 ** ((note_midi - 69) / 12.0)), 2),
            })

        contour = []
        for i in range(0, len(times), CONTOUR_STRIDE):
            if voiced[i] and f0[i] > 0:
                contour.append([round(float(times[i]), 3), round(float(f0[i]), 2)])

        payload = {
            "version": 1,
            "duration": round(float(len(y)) / float(sr), 3),
            "f0": contour,
            "notes": notes,
        }

        tmp_fd, tmp_path = tempfile.mkstemp(dir=output_dir, suffix=".melody.json")
        try:
            with os.fdopen(tmp_fd, "w", encoding="utf-8") as f:
                json.dump(payload, f, ensure_ascii=False)
            os.replace(tmp_path, melody_path)
        except Exception:
            if os.path.isfile(tmp_path):
                os.remove(tmp_path)
            raise

        return melody_path
    except Exception as e:
        print(f"[nightingale:LOG] Melody extraction failed: {e}", flush=True)
        return None
