---
title: Vocal Trainer
---

Nightingale already scores your singing live during playback and can show a full-screen pitch graph with scrolling target notes ([Pitch Scoring](/docs/scoring/)). The **Note Highway** is a dedicated practice view built on the same live microphone tracking, designed to be read at a glance: one big highway instead of several small panels.

## What it shows

- A wide lane of target notes scrolls toward you while your live microphone pitch trace glows along the highway, so flat and sharp moments are visible without leaving the song.
- As each note's window passes, it gets a Joysound-style judgment based on how close your pitch stayed: **PERFECT!** (within 25 cents), **GREAT** (50), **GOOD** (90), or **MISS**.
- Consecutive hits build a combo; the multiplier ramps up to 4x as the combo grows, and points from each judgment scale with it.
- A running score and a live grade badge (**D** through **S**) stay on screen the whole time.
- Octave snapping is anchored to the song's melody, so low male vocals are judged in their real octave instead of being read as an octave off.

## When it appears

The highway needs per-note melody data from analysis. It shows for songs analyzed with the current pipeline; for older analyses or stemless sources it stays hidden — re-analyze the song to get it.

Its size follows your playback scale and HUD position settings, just like the rest of the playback UI.
