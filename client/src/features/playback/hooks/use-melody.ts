import { useEffect, useState } from 'react';

import { loadMelody } from '@/bridge/playback';
import type { Melody } from '@/types/Melody';

/**
 * Loads the extracted melody (target notes + F0 contour) for a song.
 * Resolves to null when the song has no melody file yet: pre-feature
 * analyses or stemless sources, in which case the note highway hides.
 */
export function useMelody(fileHash: string): Melody | null {
  const [melody, setMelody] = useState<Melody | null>(null);

  useEffect(() => {
    let cancelled = false;

    const load = async (): Promise<void> => {
      try {
        const m = await loadMelody(fileHash);
        if (!cancelled) {
          setMelody(m);
        }
      } catch {
        if (!cancelled) {
          setMelody(null);
        }
      }
    };

    void load();

    return () => {
      cancelled = true;
    };
  }, [fileHash]);

  return melody;
}
