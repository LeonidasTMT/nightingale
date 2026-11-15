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
 * song: letter grade plus points.
 */
export const HighwayBestBadge = memo(({ best, className }: HighwayBestBadgeProps) => {
  const { t } = useTranslation();
  const points = formatNumber(best.points);
  return (
    <span
      title={t('library.songList.highwayBestTooltip', { grade: best.grade, points })}
      className={cn(
        'inline-flex shrink-0 items-center gap-1 rounded bg-primary/10 px-1.5 py-0.5 text-[10px] leading-none font-bold',
        className,
      )}
    >
      <span>{best.grade}</span>
      <span className="font-medium tabular-nums opacity-80">{points}</span>
    </span>
  );
});

HighwayBestBadge.displayName = 'HighwayBestBadge';
