import { useMemo } from 'react';

import { useProfiles } from '@/features/profiles/queries/use-profiles';

export function useBestScoresBySongForActiveProfile(): Map<string, number> {
  const { data } = useProfiles();
  const active = data?.active;
  const scores = data?.scores;

  return useMemo(() => {
    const map = new Map<string, number>();
    // `""` = the no-profile bucket (see ProfileStore::resolve_score_profile).
    const activeName = typeof active === 'string' && active !== '' ? active : '';

    for (const r of scores ?? []) {
      if (r.profile !== activeName) {
        continue;
      }

      const prev = map.get(r.song_hash);
      if (prev === undefined || r.score > prev) {
        map.set(r.song_hash, r.score);
      }
    }

    return map;
  }, [active, scores]);
}
