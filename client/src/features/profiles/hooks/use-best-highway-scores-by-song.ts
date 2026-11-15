import { useMemo } from 'react';

import { useProfiles } from '@/features/profiles/queries/use-profiles';

export type BestHighwayScore = { points: number; grade: string };

/**
 * Best Note Highway (vocal trainer) arcade result per song for the active
 * profile, keyed by song file hash.
 */
export function useBestHighwayScoresBySongForActiveProfile(): Map<string, BestHighwayScore> {
  const { data } = useProfiles();
  const active = data?.active;
  const records = data?.highway_scores;

  return useMemo(() => {
    const map = new Map<string, BestHighwayScore>();
    if (typeof active !== 'string' || active === '') {
      return map;
    }

    for (const r of records ?? []) {
      if (r.profile !== active) {
        continue;
      }

      const prev = map.get(r.song_hash);
      if (prev === undefined || r.points > prev.points) {
        map.set(r.song_hash, { points: r.points, grade: r.grade });
      }
    }

    return map;
  }, [active, records]);
}
