import { useMemo } from 'react';

import { useProfiles } from '@/features/profiles/queries/use-profiles';

export type BestHighwayScore = {
  points: number;
  grade: string;
  pitchGrade?: string;
  rhythmGrade?: string;
  steadyGrade?: string;
};

/**
 * Best Note Highway (vocal trainer) arcade result per song for the active
 * profile, keyed by song file hash. When no profiles exist (single-user
 * setups that never created any), scores saved in the `""` bucket are used.
 */
export function useBestHighwayScoresBySongForActiveProfile(): Map<string, BestHighwayScore> {
  const { data } = useProfiles();
  const active = data?.active;
  const records = data?.highway_scores;

  return useMemo(() => {
    const map = new Map<string, BestHighwayScore>();
    // `""` = the no-profile bucket (see ProfileStore::resolve_score_profile).
    const activeName = typeof active === 'string' && active !== '' ? active : '';

    for (const r of records ?? []) {
      if (r.profile !== activeName) {
        continue;
      }

      const prev = map.get(r.song_hash);
      if (prev === undefined || r.points > prev.points) {
        map.set(r.song_hash, {
          points: r.points,
          grade: r.grade,
          pitchGrade: r.pitch_grade,
          rhythmGrade: r.rhythm_grade,
          steadyGrade: r.steady_grade,
        });
      }
    }

    return map;
  }, [active, records]);
}
