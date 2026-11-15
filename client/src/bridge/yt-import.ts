import type { YtCookieStatus } from '@/types/YtCookieStatus';
import type { YtImportProgress } from '@/types/YtImportProgress';
import type { YtSearchResult } from '@/types/YtSearchResult';
import type { YtSubLanguage } from '@/types/YtSubLanguage';

import { invoke, listen } from './runtime';

export const fetchYtCookieStatus = async (): Promise<YtCookieStatus> => {
  return await invoke<YtCookieStatus>('yt_cookie_status', {});
};

/** Search YouTube for candidate songs (flat playlist — no per-video fetch). */
export const searchYouTube = async (query: string): Promise<YtSearchResult[]> => {
  return await invoke<YtSearchResult[]>('search_youtube', { query });
};

/** List the caption tracks a video offers, so the user can pick the lyrics language. */
export const fetchYtSubtitleLanguages = async (input: string): Promise<YtSubLanguage[]> => {
  return await invoke<YtSubLanguage[]>('youtube_subtitle_languages', { url: input });
};

/**
 * Start importing a YouTube video (id or URL) as mp3 into the library folder.
 * `captionLang` pins the caption track that becomes the lyrics; omit it to let
 * the server pick the language matching the audio. Returns immediately;
 * progress arrives over events until done/error fires.
 */
export const importYouTube = async (input: string, captionLang?: string): Promise<void> => {
  await invoke('import_youtube', { input, captionLang });
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
