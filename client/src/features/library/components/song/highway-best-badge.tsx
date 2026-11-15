import { memo } from 'react';

import type { BestHighwayScore } from '@/features/profiles/hooks/use-best-highway-scores-by-song';
import { formatNumber, useTranslation } from '@/i18n';
import { cn } from '@/shared/utils/cn';

type HighwayBestBadgeProps = {
  best: BestHighwayScore;
  className?: string;
};

/**
 * Compact badge for the best Note Highway (vocal trainer) arcade result on a
 * song: overall letter grade, per-aspect letters (pitch · rhythm · steady)
 * and points.
 */
export const HighwayBestBadge = memo(({ best, className }: HighwayBestBadgeProps) => {
  const { t } = useTranslation();
  const points = formatNumber(best.points);
  const aspectLetters = [best.pitchGrade ?? '', best.rhythmGrade ?? '', best.steadyGrade ?? ''];
  const hasAspects = aspectLetters.some((a) => a !== '');
  const aspectText = aspectLetters.map((a) => (a === '' ? '-' : a)).join('·');

  return (
    <span
      title={
        t('library.songList.highwayBestTooltip', { grade: best.grade, points }) +
        (hasAspects ? ` (${t('library.songList.highwayAspectLegend')})` : '')
      }
      className={cn(
        'inline-flex shrink-0 items-center gap-1 rounded bg-primary/10 px-1.5 py-0.5 text-[10px] leading-none font-bold',
        className,
      )}
    >
      <span>{best.grade}</span>
      {hasAspects ? (
        <span
          className="font-semibold opacity-70"
          title={t('library.songList.highwayAspectLegend')}
        >
          {aspectText}
        </span>
      ) : null}
      <span className="font-medium tabular-nums opacity-80">{points}</span>
    </span>
  );
});

HighwayBestBadge.displayName = 'HighwayBestBadge';
