import type { YtImportProgress } from '@/types/YtImportProgress';
import type { YtSearchResult } from '@/types/YtSearchResult';

import { invoke, listen } from './runtime';

/** Search YouTube for candidate songs (flat playlist — no per-video fetch). */
export const searchYouTube = async (query: string): Promise<YtSearchResult[]> => {
  return await invoke<YtSearchResult[]>('search_youtube', { query });
};

/**
 * Start importing a YouTube video (id or URL) as mp3 into the library folder.
 * Returns immediately; progress arrives over events until done/error fires.
 */
export const importYouTube = async (input: string): Promise<void> => {
  await invoke('import_youtube', { input });
};

export const onYtImportProgress = async (cb: (progress: YtImportProgress) => void) => {
  return await listen<YtImportProgress>('yt-import-progress', ({ payload }) => cb(payload));
};

export const onYtImportDone = async (cb: (path: string) => void) => {
  return await listen<string>('yt-import-done', ({ payload }) => cb(payload));
};

export const onYtImportError = async (cb: (error: string) => void) => {
  return await listen<string>('yt-import-error', ({ payload }) => cb(payload));
};
