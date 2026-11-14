export type MelodyNote = {
  start: number;
  end: number;
  midi: number;
  hz: number;
};

export type Melody = {
  version: number;
  duration: number;
  f0: [number, number][];
  notes: MelodyNote[];
};
