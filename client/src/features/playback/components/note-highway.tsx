import { useEffect, useMemo, useRef, useState } from 'react';

import { addHighwayScore } from '@/bridge/profile';
import { clampPlaybackScale } from '@/features/playback/lib/display-scale';
import type { PitchSeries } from '@/features/playback/lib/pitch/state';
import { freqToSemitone } from '@/features/playback/lib/pitch/state';
import {
  usePlaybackTransportActions,
  usePlaybackTransportState,
  usePlaybackTranscriptState,
} from '@/features/playback/providers';
import type { Melody, MelodyNote } from '@/types/Melody';
import type { Song } from '@/types/Song';

/**
 * Vocal-trainer style note highway (Joysound-like): the notes you are
 * supposed to sing (extracted from the vocal stem) scroll right-to-left
 * against your live mic pitch.
 *
 * - The FULL melody range is always visible: the vertical mapping never
 *   re-centers, so target notes stay at a fixed height for the whole song
 *   while time scrolls horizontally.
 * - Mic samples are placed at their TRUE capture timestamps (push cadence
 *   is frame-quantized), so the past stays locked to the same clock as the
 *   scrolling notes instead of drifting.
 * - Gamified like Joysound: per-sample judgments (PERFECT/GREAT/GOOD/MISS),
 *   combo with multiplier, running score and a live grade badge.
 *
 * Notes are stored in the original timeline and mapped here with
 * `time / tempo` + the song's key offset, matching the shifted stems that
 * playback actually plays. Rendering runs on requestAnimationFrame so the
 * scroll is smooth even though pitch samples arrive at ~30 Hz.
 */

const NOTE_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];

const BASE_DISPLAY_WIDTH = 480;
const BASE_DISPLAY_HEIGHT = 120;
const REFERENCE_HEIGHT = 1080;
const MIN_DISPLAY_SCALE = 0.85;

const PAST_SEC = 2;
const FUTURE_SEC = 7;
const SPAN_SEC = PAST_SEC + FUTURE_SEC;

const SEMI_PX = 8; // target vertical px per semitone at scale 1
const MAX_PLOT_H = 300; // cap so a very wide range compresses instead of overflowing
const TRACE_SPAN_SEC = 2; // how much of the mic trace to draw

// Display smoothing for the mic line. The engine already applies an EMA;
// these are render-side only and keep scoring untouched.
const MAX_SLEW_PER_SAMPLE = 1.25; // max semitone jump kept between consecutive samples
const SNAP_WINDOW_SEC = 0.75; // snap octave to a target note this close in time

type Rgb = { r: number; g: number; b: number };

const NOTE_COLOR: Rgb = { r: 0.45, g: 0.65, b: 1.0 };
const CURRENT_NOTE_COLOR: Rgb = { r: 0.75, g: 0.9, b: 1.0 };
const TRACE_IDLE: Rgb = { r: 0.8, g: 0.85, b: 1.0 };

type MappedNote = {
  start: number;
  end: number;
  midi: number;
  hz: number;
};

// Judgment (cents off the target note, octave-folded by snapping).
const PERFECT_CENTS = 25;
const GREAT_CENTS = 50;
const GOOD_CENTS = 90;
const JUDGE_WINDOW_SEC = 0.35; // sample must be this close to a target note

const PERFECT_COLOR: Rgb = { r: 1.0, g: 0.85, b: 0.2 };
const GREAT_COLOR: Rgb = { r: 0.3, g: 0.95, b: 0.4 };
const GOOD_COLOR: Rgb = { r: 0.4, g: 0.75, b: 1.0 };
const OK_COLOR: Rgb = { r: 0.98, g: 0.85, b: 0.2 };
const MISS_COLOR: Rgb = { r: 0.9, g: 0.25, b: 0.25 };

type Popup = { text: string; color: Rgb; t: number; semi: number; bornAt: number };
type Burst = { t: number; semi: number; bornAt: number };
type Judgment = { text: string; color: Rgb; points: number };
type Pt = { x: number; y: number; color: Rgb };

type GameState = {
  songKey: string;
  score: number;
  shownScore: number;
  combo: number;
  bestCombo: number;
  /** Best score still attainable from the samples seen so far (all PERFECT run). */
  maxEarnable: number;
  /** Combo an all-PERFECT player would have after each judged sample. */
  perfectCombo: number;
  /** Playback time of the last judged sample (the buffer is a shifting ring). */
  lastT: number;
  lastJudgePopupAt: number;
  lastMissPopupAt: number;
  lastComboChangeAt: number;
  popups: Popup[];
  bursts: Burst[];
};

function newGame(songKey: string): GameState {
  return {
    songKey,
    score: 0,
    shownScore: 0,
    combo: 0,
    bestCombo: 0,
    maxEarnable: 0,
    perfectCombo: 0,
    lastT: -Infinity,
    lastJudgePopupAt: -Infinity,
    lastMissPopupAt: -Infinity,
    lastComboChangeAt: -Infinity,
    popups: [],
    bursts: [],
  };
}

function midiToName(midi: number): string {
  const name = NOTE_NAMES[((midi % 12) + 12) % 12];
  return `${name}${Math.floor(midi / 12) - 1}`;
}

function rgba(color: Rgb, alpha: number): string {
  return `rgba(${Math.round(color.r * 255)}, ${Math.round(color.g * 255)}, ${Math.round(
    color.b * 255,
  )}, ${alpha})`;
}

function snapToRefOctave(refSemi: number, userSemi: number): number {
  const d = userSemi - refSemi;
  const octaveOffset = Math.round(d / 12) * 12;
  return userSemi - octaveOffset;
}

function refineSemi(hz: number, target: MappedNote | null, prevDisp: number | null): number {
  let semi = freqToSemitone(hz);
  if (target !== null) {
    // Always snap into the melody's octave so low male vocals never
    // flicker an octave off between notes.
    semi = snapToRefOctave(target.midi, semi);
  }
  // Slew limit: kill residual detector spikes (worst on male range).
  if (prevDisp !== null) {
    semi =
      prevDisp + Math.max(-MAX_SLEW_PER_SAMPLE, Math.min(MAX_SLEW_PER_SAMPLE, semi - prevDisp));
  }
  return semi;
}

function noteAlpha(n: MappedNote, now: number): number {
  if (now >= n.start && now < n.end) {
    return 1;
  }
  const age = Math.max(0, now - n.end);
  return Math.max(0.25, 0.8 - age * 0.3);
}

function centsColor(cents: number): Rgb {
  const a = Math.abs(cents);
  if (a <= GREAT_CENTS) {
    return GREAT_COLOR;
  }
  if (a <= GOOD_CENTS) {
    return OK_COLOR;
  }
  return MISS_COLOR;
}

function comboMultiplier(combo: number): number {
  if (combo >= 80) {
    return 4;
  }
  if (combo >= 40) {
    return 3;
  }
  if (combo >= 20) {
    return 2;
  }
  return 1;
}

type Grade = { letter: string; color: Rgb };

/**
 * Letter grade for a ratio of earned points over the best score attainable
 * at that moment (never the whole-song maximum, so the badge stays honest
 * mid-song).
 */
function liveGrade(score: number | null): Grade | null {
  if (score === null || score <= 0) {
    return null;
  }
  if (score >= 0.97) {
    return { letter: 'SSS', color: PERFECT_COLOR };
  }
  if (score >= 0.92) {
    return { letter: 'S', color: PERFECT_COLOR };
  }
  if (score >= 0.85) {
    return { letter: 'A', color: GREAT_COLOR };
  }
  if (score >= 0.7) {
    return { letter: 'B', color: GOOD_COLOR };
  }
  if (score >= 0.5) {
    return { letter: 'C', color: PERFECT_COLOR };
  }
  return { letter: 'D', color: MISS_COLOR };
}

function mapMelody(
  melody: Melody,
  tempo: number,
  keyShift: number,
): {
  notes: MappedNote[];
  minMidi: number;
  maxMidi: number;
} {
  const notes: MappedNote[] = melody.notes.map((n: MelodyNote) => ({
    start: n.start / tempo,
    end: n.end / tempo,
    midi: n.midi + keyShift,
    hz: n.hz * Math.pow(2, keyShift / 12),
  }));
  let minMidi = Infinity;
  let maxMidi = -Infinity;
  for (const n of notes) {
    if (n.midi < minMidi) {
      minMidi = n.midi;
    }
    if (n.midi > maxMidi) {
      maxMidi = n.midi;
    }
  }
  return { notes, minMidi, maxMidi };
}

type Layout = {
  width: number;
  height: number;
  padX: number;
  padY: number;
  plotW: number;
  plotH: number;
  lo: number;
  hi: number;
};

function computeLayout(opts: {
  windowWidth: number;
  windowHeight: number;
  userScale: number;
  minMidi: number;
  maxMidi: number;
}): Layout {
  const scale = Math.max(MIN_DISPLAY_SCALE, opts.windowHeight / REFERENCE_HEIGHT) * opts.userScale;
  // Stable full-range window: fixed for the whole song, no recentering.
  const lo = Math.floor(opts.minMidi) - 1;
  const hi = Math.ceil(opts.maxMidi) + 1;
  const range = hi - lo;

  const width = Math.min(BASE_DISPLAY_WIDTH * scale, Math.max(300, opts.windowWidth - 32));
  const padX = 10 * scale;
  const padY = 8 * scale;

  // Grow the plot so each semitone gets ~SEMI_PX px, within bounds.
  let plotH = range * SEMI_PX * scale;
  plotH = Math.max(BASE_DISPLAY_HEIGHT - padY * 2, plotH);
  plotH = Math.min(plotH, MAX_PLOT_H * scale);

  const height = plotH + padY * 2;
  return { width, height, padX, padY, plotW: width - padX * 2, plotH, lo, hi };
}

export type NoteHighwayProps = {
  melody: Melody | null;
  song: Song;
  series: PitchSeries;
  position?: 'top' | 'bottom';
  scale?: number | null;
};

export function NoteHighway({
  melody,
  song,
  series,
  position = 'top',
  scale = 1,
}: NoteHighwayProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const { getCurrentTime } = usePlaybackTransportActions();
  const [size, setSize] = useState(() => ({
    height: window.innerHeight,
    width: window.innerWidth,
  }));

  // Latest values read by the rAF loop without restarting it.
  const seriesRef = useRef<PitchSeries>(series);
  const timeFnRef = useRef(getCurrentTime);
  const gameRef = useRef<GameState | null>(null);

  useEffect(() => {
    seriesRef.current = series;
    timeFnRef.current = getCurrentTime;
  });

  // Persist the run when the song finishes (or the outro is skipped): the
  // best arcade result per song + active profile, graded against the best
  // score attainable at that moment.
  const { isFinished } = usePlaybackTransportState();
  const { skipOutroPending } = usePlaybackTranscriptState();
  const savedRef = useRef(false);

  useEffect(() => {
    if (!isFinished && !skipOutroPending) {
      return;
    }
    const game = gameRef.current;
    if (savedRef.current || game === null || game.score <= 0) {
      return;
    }
    savedRef.current = true;
    const ratio = game.maxEarnable > 0 ? game.score / game.maxEarnable : 0;
    const grade = liveGrade(ratio);
    if (grade !== null) {
      void addHighwayScore(song.file_hash, Math.floor(game.score), grade.letter);
    }
  }, [isFinished, skipOutroPending, song.file_hash]);

  const tempo = song.tempo > 0 ? song.tempo : 1;
  const keyShift = song.override_key === null ? 0 : song.key_offset;

  const mapped = useMemo(
    () => (melody ? mapMelody(melody, tempo, keyShift) : null),
    [melody, tempo, keyShift],
  );

  useEffect(() => {
    const onResize = () => setSize({ height: window.innerHeight, width: window.innerWidth });
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);

  useEffect(() => {
    let raf = 0;

    if (mapped !== null) {
      const canvas = canvasRef.current;
      const ctx = canvas === null ? null : canvas.getContext('2d');
      if (canvas !== null && ctx !== null) {
        const layout = computeLayout({
          windowWidth: size.width,
          windowHeight: size.height,
          userScale: clampPlaybackScale(scale),
          minMidi: mapped.minMidi,
          maxMidi: mapped.maxMidi,
        });
        const dpr = window.devicePixelRatio || 1;
        canvas.width = Math.floor(layout.width * dpr);
        canvas.height = Math.floor(layout.height * dpr);
        canvas.style.width = `${layout.width}px`;
        canvas.style.height = `${layout.height}px`;
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

        const { notes } = mapped;
        if (notes.length > 0) {
          const songKey = `${song.title}|${song.artist}`;
          const existing = gameRef.current;
          const isNewGame = existing === null || existing.songKey !== songKey;
          const game = isNewGame ? newGame(songKey) : existing;
          gameRef.current = game;
          savedRef.current = isNewGame ? false : savedRef.current;

          const semiPx = layout.plotH / (layout.hi - layout.lo);
          const scaleF = clampPlaybackScale(scale);
          const fontBase = Math.max(9, 10 * scaleF);
          const barH = Math.min(Math.max(3, 0.7 * semiPx), 10 * scaleF);
          const yAt = (midi: number): number =>
            layout.height -
            layout.padY -
            ((midi - layout.lo) / (layout.hi - layout.lo)) * layout.plotH;

          /** Nearest target note in time (previous or upcoming), within window. */
          const nearestNote = (t: number, windowSec: number): MappedNote | null => {
            let ptr = 0;
            while (ptr < notes.length && notes[ptr].end <= t) {
              ptr += 1;
            }
            let best: MappedNote | null = null;
            let bestDist = Infinity;
            const candidates: (MappedNote | undefined)[] = [notes[ptr - 1], notes[ptr]];
            for (const n of candidates) {
              if (n === undefined) {
                continue;
              }
              let dist = 0;
              if (t < n.start) {
                dist = n.start - t;
              } else if (t > n.end) {
                dist = t - n.end;
              }
              if (dist < bestDist) {
                bestDist = dist;
                best = n;
              }
            }
            if (best !== null && bestDist <= windowSec) {
              return best;
            }
            return null;
          };

          const classifyJ = (cents: number): Judgment | null => {
            if (cents <= PERFECT_CENTS) {
              return { text: 'PERFECT!', color: PERFECT_COLOR, points: 300 };
            }
            if (cents <= GREAT_CENTS) {
              return { text: 'GREAT', color: GREAT_COLOR, points: 150 };
            }
            if (cents <= GOOD_CENTS) {
              return { text: 'GOOD', color: GOOD_COLOR, points: 50 };
            }
            return null;
          };

          const pushPopup = (popup: Popup): void => {
            game.popups.push(popup);
          };

          const judgeSample = (hz: number | null, t: number, wallMs: number): void => {
            const target = nearestNote(t, JUDGE_WINDOW_SEC);
            if (target === null) {
              return; // intro/outro: no judgment, no combo change
            }
            // This sample IS judgeable, so an all-PERFECT run would score on
            // it too: grow the progressive maximum by exactly that.
            game.perfectCombo += 1;
            game.maxEarnable += 300 * comboMultiplier(game.perfectCombo);
            if (hz === null) {
              // Silence only counts as a miss while inside a note; breaths
              // before/after phrases are not penalized.
              if (t < target.start || t > target.end) {
                return;
              }
              game.combo = 0;
              game.lastComboChangeAt = wallMs;
              if (wallMs - game.lastMissPopupAt > 500) {
                game.lastMissPopupAt = wallMs;
                pushPopup({
                  text: 'MISS',
                  color: MISS_COLOR,
                  t,
                  semi: target.midi,
                  bornAt: wallMs,
                });
              }
              return;
            }
            let semi = freqToSemitone(hz);
            semi = snapToRefOctave(target.midi, semi);
            const cents = Math.abs(100 * (semi - freqToSemitone(target.hz)));
            const judgment = classifyJ(cents);
            if (judgment !== null) {
              game.combo += 1;
              game.score += judgment.points * comboMultiplier(game.combo);
              // Throttle popups so ~30 samples/sec don't spam the screen.
              if (wallMs - game.lastJudgePopupAt > 280) {
                game.lastJudgePopupAt = wallMs;
                pushPopup({
                  text: judgment.text,
                  color: judgment.color,
                  t,
                  semi: target.midi,
                  bornAt: wallMs,
                });
                if (cents <= PERFECT_CENTS) {
                  game.bursts.push({ t, semi: target.midi, bornAt: wallMs });
                }
              }
            } else {
              game.combo = 0;
            }
            game.lastComboChangeAt = wallMs;
            if (game.combo > game.bestCombo) {
              game.bestCombo = game.combo;
            }
          };

          const processSamples = (now: number, wallMs: number): void => {
            const userPitches = seriesRef.current.userPitches;
            const times = seriesRef.current.times;
            const len = userPitches.length;
            if (times.length !== len || len === 0) {
              return;
            }
            const newest = times[len - 1];
            if (newest < game.lastT - 0.5) {
              // Playback time went backwards (replay / seek): start the game over.
              game.lastT = -Infinity;
              game.score = 0;
              game.shownScore = 0;
              game.combo = 0;
              game.bestCombo = 0;
              game.maxEarnable = 0;
              game.perfectCombo = 0;
              savedRef.current = false;
              game.popups.length = 0;
              game.bursts.length = 0;
            }
            for (let i = 0; i < len; i += 1) {
              const t = times[i];
              if (t <= game.lastT) {
                continue;
              }
              // Judge only recent samples; older ones are still marked processed.
              if (now - t <= TRACE_SPAN_SEC + 1) {
                judgeSample(userPitches[i], t, wallMs);
              }
              game.lastT = t;
            }
          };

          const drawGrid = (): void => {
            ctx.font = `${fontBase}px ui-monospace, monospace`;
            for (let m = Math.ceil(layout.lo); m <= layout.hi; m += 1) {
              if (((m % 12) + 12) % 12 !== 0) {
                continue;
              }
              const y = yAt(m);
              ctx.strokeStyle = 'rgba(255, 255, 255, 0.08)';
              ctx.lineWidth = 1;
              ctx.beginPath();
              ctx.moveTo(layout.padX, y);
              ctx.lineTo(layout.width - layout.padX, y);
              ctx.stroke();
              ctx.fillStyle = 'rgba(255, 255, 255, 0.35)';
              ctx.fillText(midiToName(m), layout.padX + 2, y - 3);
            }
          };

          const drawNotes = (now: number, xAt: (t: number) => number): void => {
            for (const n of notes) {
              if (n.end < now - PAST_SEC || n.start > now + FUTURE_SEC) {
                continue;
              }
              const x1 = Math.max(layout.padX, xAt(n.start));
              const x2 = Math.min(layout.width - layout.padX, xAt(n.end));
              if (x2 <= x1) {
                continue;
              }
              const y = yAt(n.midi);
              const isCurrent = now >= n.start && now < n.end;
              const alpha = noteAlpha(n, now);
              const h = isCurrent ? barH * 1.6 : barH;
              ctx.fillStyle = rgba(isCurrent ? CURRENT_NOTE_COLOR : NOTE_COLOR, alpha);
              if (isCurrent) {
                ctx.shadowColor = rgba(CURRENT_NOTE_COLOR, 0.9);
                ctx.shadowBlur = 8 * scaleF;
              }
              ctx.beginPath();
              ctx.roundRect(x1, y - h / 2, x2 - x1, h, h / 2);
              ctx.fill();
              ctx.shadowBlur = 0;
            }
          };

          const drawCurrentLabel = (now: number, xAt: (t: number) => number): void => {
            const n = notes.find((note) => now >= note.start && now < note.end);
            if (n === undefined) {
              return;
            }
            const x1 = Math.max(layout.padX, xAt(n.start));
            const y = yAt(n.midi);
            ctx.fillStyle = 'rgba(255, 255, 255, 0.9)';
            ctx.fillText(midiToName(n.midi), x1 + 4, y - barH * 0.8 - 3);
          };

          const drawNowLine = (nx: number): void => {
            ctx.strokeStyle = 'rgba(255, 255, 255, 0.5)';
            ctx.lineWidth = 1.5;
            ctx.beginPath();
            ctx.moveTo(nx, layout.padY / 2);
            ctx.lineTo(nx, layout.height - layout.padY / 2);
            ctx.stroke();
          };

          const drawBursts = (wallMs: number, xAt: (t: number) => number): void => {
            game.bursts = game.bursts.filter((b) => wallMs - b.bornAt < 500);
            for (const b of game.bursts) {
              const age = (wallMs - b.bornAt) / 500;
              const x = xAt(b.t);
              if (x < layout.padX || x > layout.width - layout.padX) {
                continue;
              }
              ctx.strokeStyle = rgba(PERFECT_COLOR, (1 - age) * 0.8);
              ctx.lineWidth = 2 * scaleF;
              ctx.beginPath();
              ctx.arc(x, yAt(b.semi), (4 + 26 * age) * scaleF, 0, Math.PI * 2);
              ctx.stroke();
            }
          };

          const collectTracePoints = (now: number, xAt: (t: number) => number): Pt[] => {
            const pts: Pt[] = [];
            const userPitches = seriesRef.current.userPitches;
            const times = seriesRef.current.times;
            const len = userPitches.length;
            if (len < 2 || times.length !== len) {
              return pts;
            }
            let prevDisp: number | null = null;
            for (let i = 0; i < len; i += 1) {
              const hz = userPitches[i];
              if (hz === null) {
                continue;
              }
              const t = times[i];
              if (t < now - TRACE_SPAN_SEC) {
                continue;
              }
              const target = nearestNote(t, SNAP_WINDOW_SEC);
              const semi = refineSemi(hz, target, prevDisp);
              prevDisp = semi;
              const x = xAt(t);
              if (x < layout.padX) {
                continue;
              }
              const cents = target !== null ? 100 * (semi - freqToSemitone(target.hz)) : 0;
              pts.push({
                x,
                y: yAt(semi),
                color: target !== null ? centsColor(cents) : TRACE_IDLE,
              });
            }
            return pts;
          };

          const drawTrace = (pts: Pt[]): void => {
            if (pts.length < 2) {
              return;
            }
            // Glow pass.
            ctx.lineJoin = 'round';
            ctx.lineCap = 'round';
            ctx.strokeStyle = rgba(TRACE_IDLE, 0.22);
            ctx.lineWidth = 6 * scaleF;
            ctx.beginPath();
            ctx.moveTo(pts[0].x, pts[0].y);
            for (let i = 1; i < pts.length; i += 1) {
              ctx.lineTo(pts[i].x, pts[i].y);
            }
            ctx.stroke();
            // Colored core pass (segment by segment).
            ctx.lineWidth = 2.5 * scaleF;
            for (let i = 1; i < pts.length; i += 1) {
              ctx.strokeStyle = rgba(pts[i].color, 0.95);
              ctx.beginPath();
              ctx.moveTo(pts[i - 1].x, pts[i - 1].y);
              ctx.lineTo(pts[i].x, pts[i].y);
              ctx.stroke();
            }
            const tip = pts[pts.length - 1];
            ctx.fillStyle = rgba(tip.color, 1);
            ctx.beginPath();
            ctx.arc(tip.x, tip.y, 3 * scaleF, 0, Math.PI * 2);
            ctx.fill();
          };

          const drawPopups = (wallMs: number, xAt: (t: number) => number): void => {
            game.popups = game.popups.filter((p) => wallMs - p.bornAt < 900);
            for (const p of game.popups) {
              const age = (wallMs - p.bornAt) / 900;
              const x = xAt(p.t);
              if (x < layout.padX || x > layout.width - layout.padX) {
                continue;
              }
              ctx.font = `bold ${Math.max(10, 12 * scaleF)}px ui-monospace, monospace`;
              ctx.fillStyle = rgba(p.color, 1 - age);
              ctx.fillText(p.text, x + 4, yAt(p.semi) - 8 - 14 * age * scaleF);
            }
          };

          const drawHud = (now: number, wallMs: number): void => {
            const currentNote = notes.find((n) => now >= n.start && now < n.end);
            if (currentNote !== undefined) {
              ctx.font = `bold ${Math.max(11, 13 * scaleF)}px ui-monospace, monospace`;
              ctx.fillStyle = 'rgba(255, 255, 255, 0.85)';
              ctx.fillText(midiToName(currentNote.midi), layout.padX + 2, layout.padY + 4);
            }

            game.shownScore += (game.score - game.shownScore) * 0.15;
            if (Math.abs(game.score - game.shownScore) < 1) {
              game.shownScore = game.score;
            }
            const ratio = game.maxEarnable > 0 ? game.score / game.maxEarnable : null;
            const grade = liveGrade(ratio);
            ctx.font = `bold ${Math.max(11, 13 * scaleF)}px ui-monospace, monospace`;
            const scoreText = `${grade !== null ? grade.letter + '  ' : ''}${Math.floor(game.shownScore).toLocaleString()}`;
            ctx.fillStyle = 'rgba(255, 255, 255, 0.9)';
            ctx.fillText(
              scoreText,
              layout.width - layout.padX - ctx.measureText(scoreText).width,
              layout.padY + 4,
            );

            if (game.combo > 1) {
              const sinceChange = wallMs - game.lastComboChangeAt;
              const pulse = Math.max(0, 1 - sinceChange / 250);
              const comboFont = `${Math.max(13, 16 * scaleF) * (1 + 0.25 * pulse)}px`;
              ctx.font = `bold ${comboFont} ui-monospace, monospace`;
              const comboText = `x${game.combo}`;
              const mult = comboMultiplier(game.combo);
              const label = mult > 1 ? `${comboText}  ${mult}x` : comboText;
              ctx.fillStyle = rgba(mult >= 3 ? PERFECT_COLOR : TRACE_IDLE, 0.95);
              ctx.fillText(
                label,
                layout.width - layout.padX - ctx.measureText(label).width,
                layout.height - layout.padY / 2,
              );
            }
          };

          const render = (): void => {
            const now = timeFnRef.current();
            const wallMs = performance.now();
            const xAt = (t: number): number =>
              layout.padX + ((t - (now - PAST_SEC)) / SPAN_SEC) * layout.plotW;

            ctx.clearRect(0, 0, layout.width, layout.height);
            drawGrid();
            drawNotes(now, xAt);
            drawCurrentLabel(now, xAt);
            drawNowLine(xAt(now));
            processSamples(now, wallMs);
            drawBursts(wallMs, xAt);
            const pts = collectTracePoints(now, xAt);
            drawTrace(pts);
            drawPopups(wallMs, xAt);
            drawHud(now, wallMs);
          };

          const loop = (): void => {
            render();
            raf = requestAnimationFrame(loop);
          };
          raf = requestAnimationFrame(loop);
        }
      }
    }

    return () => cancelAnimationFrame(raf);
  }, [mapped, size, scale, song.title, song.artist]);

  if (!melody) {
    return null;
  }

  const positionClass = position === 'bottom' ? 'bottom-14 sm:bottom-16' : 'top-14 sm:top-16';

  return (
    <div
      className={`pointer-events-none absolute left-1/2 z-20 -translate-x-1/2 rounded-sm border-white/15 bg-black/40 p-1 ${positionClass}`}
    >
      <canvas ref={canvasRef} className="block" />
    </div>
  );
}
