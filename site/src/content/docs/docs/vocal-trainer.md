---
title: Vocal Trainer
---

Nightingale already scores your singing live during playback and can show a full-screen pitch graph with scrolling target notes ([Pitch Scoring](/docs/scoring/)). The **Note Highway** is a dedicated practice view built on the same live microphone tracking, designed to be read at a glance: one big highway instead of several small panels.

## What it shows

- A wide lane of target notes scrolls toward you while your live microphone pitch trace glows along the highway, so flat and sharp moments are visible without leaving the song.
- As each note's window passes, it gets a Joysound-style judgment based on how close your pitch stayed: **PERFECT!** (within 40 cents), **GREAT** (70), **GOOD** (120 — a full semitone off still counts), or **MISS**. The windows are tuned so being *in tune* is enough to clear D; PERFECT stays the tight pocket for clean runs.
- Consecutive hits build a combo; the multiplier ramps up to 4x as the combo grows, and points from each judgment scale with it (300 / 200 / 100 per PERFECT / GREAT / GOOD).
- A running score and a live grade badge (**D** through **SSS**) stay on screen the whole time.
- Under the score, three aspect letters appear as soon as there is enough sung: **pitch** (mean accuracy in cents), **rhythm** (when you start notes + how much of each note you actually sing) and **steady** (how tightly you hold a pitch). They grade *why* your overall grade is what it is — e.g. an A overall with a D on rhythm means you're in tune but rushing the notes.
- Octave snapping is anchored to the song's melody, so low male vocals are judged in their real octave instead of being read as an octave off.

## When it appears

The highway needs per-note melody data from analysis. It shows for songs analyzed with the current pipeline; for older analyses or stemless sources it stays hidden — re-analyze the song to get it.

Its size follows your playback scale and HUD position settings, just like the rest of the playback UI.

## How it differs from the standard pitch scoring

Both views run on the same live microphone tracking, but they grade you differently:

| | [Standard pitch scoring](/docs/scoring/) | Note Highway |
| --- | --- | --- |
| Target | The reference vocal itself — its pitch is re-detected every frame from the stem audio, so the target is a continuous curve of "the exact note being sung right now" | Discrete melody notes extracted by analysis; each note is a bar with a start time, end time, and pitch |
| Measuring closeness | Continuous similarity: 1.0 when your pitch matches, ramping linearly to 0 six semitones away (octave-folded); every moment of singable time contributes in proportion | Per-note judgment: each mic sample is graded against the nearest note within ±0.35 s — PERFECT ≤40 ¢, GREAT ≤70 ¢, GOOD ≤120 ¢; worse than that breaks the combo, and silence inside a note is a visible MISS (breaths between phrases are not penalized) |
| Score model | Time-weighted accuracy integral: no events, no combo — your score is how much of the singable time you stayed close to the singer | Rhythm-game scoring: 300/200/100 points per judgment × combo multiplier (2× from 20, 3× from 40, 4× from 80), plus a running total and best combo. A run that is consistently GREAT caps around B and a PERFECT/GREAT mix reaches A, so above D is about being in tune rather than pixel-perfect |
| Live readout | A five-star row with half-star precision, and a pitch graph where your trace is colored by similarity against the reference note | One big scrolling lane: a 9 s window (2 s past / 7 s future), the full melody range always visible, notes scroll toward you, mic trace glows along the highway |
| Grade | Star rating, final at end of song | Live letter grade (**D** to **SSS**) computed against the best score still attainable *so far*, so it stays honest mid-song |
| Saved results | End-of-song results → per-song leaderboard/scoreboard, profiles, multiplayer | Best highway run saved per song (overall grade + pitch/rhythm/steady letters), shown as a badge in the library and song details; works with or without profiles — with no profile selected, runs are kept in a shared bucket |
| Needs | Any analyzed song (falls back to the original mix without stems, less accurate) | Per-note melody data from the current analysis pipeline; hidden otherwise |

In short: standard scoring grades *how well you tracked the singer* over the whole song; the highway grades *each note as it arrives*, so you can see exactly which notes broke your combo.
