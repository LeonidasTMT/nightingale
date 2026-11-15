import { useQueryClient } from '@tanstack/react-query';
import { AlertTriangleIcon, Loader2Icon, SearchIcon, YoutubeIcon } from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import { toast } from 'sonner';

import {
  fetchYtCookieStatus,
  fetchYtSubtitleLanguages,
  importYouTube,
  onYtImportDone,
  onYtImportError,
  onYtImportProgress,
  searchYouTube,
} from '@/bridge/yt-import';
import { useDialog } from '@/features/menu/hooks/use-dialog';
import { Button } from '@/shared/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/shared/components/ui/dialog';
import { Input } from '@/shared/components/ui/input';
import { Progress } from '@/shared/components/ui/progress';
import { ScrollArea } from '@/shared/components/ui/scroll-area';
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/shared/components/ui/select';
import { ANALYSIS_QUEUE, MENU, SONGS, SONGS_META } from '@/shared/query-keys';
import type { YtCookieStatus } from '@/types/YtCookieStatus';
import type { YtImportProgress } from '@/types/YtImportProgress';
import type { YtSearchResult } from '@/types/YtSearchResult';
import type { YtSubLanguage } from '@/types/YtSubLanguage';

type View = 'search' | 'results' | 'confirm' | 'importing' | 'error';

type Flow = {
  view: View;
  query: string;
  results: YtSearchResult[];
  confirmInput: string;
  /** null while the caption list is loading (or if it failed). */
  langs: YtSubLanguage[] | null;
  /** 'auto' or one of the available language codes. */
  selectedLang: string;
  progress: YtImportProgress | null;
  error: string;
  busy: boolean;
};

const INITIAL_FLOW: Flow = {
  view: 'search',
  query: '',
  results: [],
  confirmInput: '',
  langs: null,
  selectedLang: 'auto',
  progress: null,
  error: '',
  busy: false,
};

const errorMessage = (error: unknown): string => {
  const message = error instanceof Error ? error.message : 'unknown error';
  if (/sign in|not a bot|cookies/i.test(message)) {
    return `${message} Tip: export your browser cookies for youtube.com to yt_cookies.txt (in the library folder or data dir) to bypass this.`;
  }
  return message;
};

const isCookieError = (message: string): boolean => /sign in|not a bot|cookies/i.test(message);

/** One line describing what sign-in source imports will use, with real paths. */
const cookieStatusLabel = (status: YtCookieStatus): string => {
  if (status.active !== null && status.active.startsWith('file:')) {
    return `cookie file: ${status.active.slice('file:'.length)}`;
  }
  if (status.active !== null && status.active.startsWith('browser:')) {
    return `browser cookies: ${status.active.slice('browser:'.length)}`;
  }
  const drop = status.dropPaths.length > 0 ? status.dropPaths[0] : '';
  return drop === '' ? 'none' : `none — to sign in, save your youtube.com cookies as ${drop}`;
};

const durationLabel = (sec?: number): string => {
  if (typeof sec !== 'number') {
    return '';
  }
  const minutes = Math.floor(sec / 60);
  const rest = sec % 60;
  return `${minutes}:${String(rest).padStart(2, '0')}`;
};

const looksLikeDirectInput = (raw: string): boolean => {
  const value = raw.trim();
  return /^https?:\/\//i.test(value) || /^[a-zA-Z0-9_-]{11}$/.test(value);
};

/**
 * State machine for the dialog. The flow resets when the dialog opens, using
 * the render-time "adjust state on prop change" pattern (no effect needed).
 */
const useAddFromYoutubeFlow = (open: boolean) => {
  const { close } = useDialog();
  const queryClient = useQueryClient();

  const [flow, setFlow] = useState<Flow>(INITIAL_FLOW);
  const [cookieStatus, setCookieStatus] = useState<YtCookieStatus | null>(null);
  const [wasOpen, setWasOpen] = useState(open);

  if (open && !wasOpen) {
    setWasOpen(true);
    setFlow(INITIAL_FLOW);
  } else if (!open && wasOpen) {
    setWasOpen(false);
  }

  const unsubsRef = useRef<Array<() => void>>([]);

  useEffect(() => {
    if (!open) {
      return;
    }
    // The dialog component stays mounted while closed, so a late result is
    // just re-read on the next open — no cancellation needed.
    void fetchYtCookieStatus()
      .then((status) => setCookieStatus(status))
      .catch(() => {
        // Cosmetic only — the dialog works without it.
      });
  }, [open]);

  useEffect(() => {
    const unsubs = unsubsRef.current;
    return () => {
      for (const unsub of unsubs) {
        unsub();
      }
    };
  }, []);

  const runSearch = useCallback(async (raw: string) => {
    const query = raw.trim();
    if (query === '') {
      return;
    }
    setFlow((f) => ({ ...f, busy: true }));
    try {
      const found = await searchYouTube(query);
      setFlow((f) => ({ ...f, results: found, view: 'results', busy: false }));
    } catch (error: unknown) {
      setFlow((f) => ({ ...f, busy: false, error: errorMessage(error), view: 'error' }));
    }
  }, []);

  const openConfirm = useCallback((input: string) => {
    setFlow((f) => ({
      ...f,
      view: 'confirm',
      confirmInput: input,
      langs: null,
      selectedLang: 'auto',
      error: '',
    }));
    // Best-effort: if the list fails, the import still works with Auto.
    void (async () => {
      try {
        const langs = await fetchYtSubtitleLanguages(input);
        setFlow((f) => (f.view === 'confirm' && f.confirmInput === input ? { ...f, langs } : f));
      } catch {
        // No caption list; Auto still works.
      }
    })();
  }, []);

  const setSelectedLang = useCallback((lang: string) => {
    setFlow((f) => ({ ...f, selectedLang: lang }));
  }, []);

  const startImport = useCallback(
    async (input: string, captionLang?: string) => {
      // Subscribe before invoking so no progress event can be missed.
      const unsubs = await Promise.all([
        onYtImportProgress((progress) => setFlow((f) => ({ ...f, progress }))),
        onYtImportDone((path) => {
          void queryClient.invalidateQueries({ queryKey: SONGS });
          void queryClient.invalidateQueries({ queryKey: SONGS_META });
          void queryClient.invalidateQueries({ queryKey: MENU });
          void queryClient.invalidateQueries({ queryKey: ANALYSIS_QUEUE });
          toast.success(`Added "${path.split(/[\\/]/).pop()}" to the library`);
          close();
        }),
        onYtImportError((message) => setFlow((f) => ({ ...f, error: message, view: 'error' }))),
      ]);
      unsubsRef.current = unsubs;

      setFlow((f) => ({ ...f, view: 'importing', progress: null }));
      try {
        await importYouTube(input, captionLang);
      } catch (error: unknown) {
        setFlow((f) => ({ ...f, error: errorMessage(error), view: 'error' }));
      }
    },
    [close, queryClient],
  );

  const backToSearch = useCallback(() => {
    setFlow((f) => ({ ...f, view: 'search', error: '' }));
  }, []);

  const backFromConfirm = useCallback(() => {
    setFlow((f) => ({ ...f, view: f.results.length > 0 ? 'results' : 'search', error: '' }));
  }, []);

  const setQuery = useCallback((value: string) => {
    setFlow((f) => ({ ...f, query: value }));
  }, []);

  return {
    flow,
    cookieStatus,
    runSearch,
    startImport,
    openConfirm,
    backToSearch,
    backFromConfirm,
    setSelectedLang,
    setQuery,
  };
};

type SearchViewProps = {
  flow: Flow;
  cookieStatus: YtCookieStatus | null;
  inputRef: React.RefObject<HTMLInputElement | null>;
  onQueryChange: (value: string) => void;
  onSearch: (query: string) => void;
  onDirectImport: (input: string) => void;
};

const SearchView = ({
  flow,
  cookieStatus,
  inputRef,
  onQueryChange,
  onSearch,
  onDirectImport,
}: SearchViewProps) => {
  const directInput = flow.query.trim();

  return (
    <>
      <DialogHeader>
        <DialogTitle className="flex items-center gap-2">
          <YoutubeIcon className="size-5" />
          Add from YouTube
        </DialogTitle>
        <DialogDescription>
          Search for a song and it will be downloaded as mp3 into your library folder.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          onSearch(flow.query);
        }}
      >
        <div className="flex gap-2">
          <Input
            ref={inputRef}
            value={flow.query}
            onChange={(e) => onQueryChange(e.target.value)}
            placeholder="Song, artist, or a YouTube link…"
          />
          <Button type="submit" disabled={directInput === '' || flow.busy}>
            {flow.busy ? <Loader2Icon className="animate-spin" /> : <SearchIcon />}
            Search
          </Button>
        </div>
      </form>
      {looksLikeDirectInput(directInput) && (
        <Button
          variant="outline"
          className="w-full"
          disabled={flow.busy}
          onClick={() => onDirectImport(directInput)}
        >
          Import this link directly
        </Button>
      )}
      {cookieStatus !== null && (
        <p className="break-all text-xs text-muted-foreground">
          YouTube sign-in: {cookieStatusLabel(cookieStatus)}
        </p>
      )}
    </>
  );
};

type ConfirmViewProps = {
  input: string;
  langs: YtSubLanguage[] | null;
  selectedLang: string;
  onLangChange: (lang: string) => void;
  onImport: () => void;
  onBack: () => void;
};

type CaptionPickerProps = {
  langs: YtSubLanguage[] | null;
  selectedLang: string;
  onLangChange: (lang: string) => void;
};

const CaptionPicker = ({ langs, selectedLang, onLangChange }: CaptionPickerProps) => {
  if (langs === null) {
    return (
      <p className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2Icon className="animate-spin" />
        Loading caption languages…
      </p>
    );
  }
  if (langs.length === 0) {
    return <p className="text-sm text-muted-foreground">No captions available for this video.</p>;
  }
  return (
    <Select value={selectedLang} onValueChange={(lang) => onLangChange(lang)}>
      <SelectTrigger id="youtube-import.captionLangSelect">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectGroup>
          <SelectItem value="auto">Auto — match the song language</SelectItem>
          {langs.map((l) => (
            <SelectItem key={l.lang} value={l.lang}>
              {l.lang} ({l.source === 'manual' ? 'uploaded' : 'machine'})
            </SelectItem>
          ))}
        </SelectGroup>
      </SelectContent>
    </Select>
  );
};

const ConfirmView = ({
  input,
  langs,
  selectedLang,
  onLangChange,
  onImport,
  onBack,
}: ConfirmViewProps) => (
  <>
    <DialogHeader>
      <DialogTitle className="flex items-center gap-2">
        <YoutubeIcon className="size-5" />
        Import from YouTube
      </DialogTitle>
      <DialogDescription>{input}</DialogDescription>
    </DialogHeader>
    <div className="space-y-1.5">
      <p className="text-sm text-muted-foreground">Captions (used as lyrics)</p>
      <CaptionPicker langs={langs} selectedLang={selectedLang} onLangChange={onLangChange} />
    </div>
    <DialogFooter>
      <Button variant="outline" onClick={onBack}>
        Back
      </Button>
      <Button onClick={onImport}>Import</Button>
    </DialogFooter>
  </>
);

type ResultsViewProps = {
  query: string;
  results: YtSearchResult[];
  onPick: (id: string) => void;
  onBack: () => void;
};

const ResultsView = ({ query, results, onPick, onBack }: ResultsViewProps) => (
  <>
    <DialogHeader>
      <DialogTitle className="flex items-center gap-2">
        <YoutubeIcon className="size-5" />
        Results for “{query.trim()}”
      </DialogTitle>
      <DialogDescription>Pick a song to download it as mp3.</DialogDescription>
    </DialogHeader>
    {results.length === 0 ? (
      <p className="text-sm text-muted-foreground">No results found.</p>
    ) : (
      <ScrollArea className="max-h-72 pr-2">
        <div className="space-y-1">
          {results.map((result) => (
            <Button
              key={result.id}
              variant="ghost"
              className="w-full justify-start px-2 py-1.5 text-left font-normal"
              onClick={() => onPick(result.id)}
            >
              <div className="flex w-full items-center gap-3">
                <span className="min-w-0 flex-1 truncate">{result.title}</span>
                {typeof result.duration_sec === 'number' && (
                  <span className="shrink-0 text-xs text-muted-foreground">
                    {durationLabel(result.duration_sec)}
                  </span>
                )}
              </div>
              {typeof result.uploader === 'string' && result.uploader !== '' && (
                <p className="mt-0.5 truncate text-xs text-muted-foreground">{result.uploader}</p>
              )}
            </Button>
          ))}
        </div>
      </ScrollArea>
    )}
    <DialogFooter>
      <Button variant="outline" onClick={onBack}>
        Back
      </Button>
    </DialogFooter>
  </>
);

type ImportingViewProps = { progress: YtImportProgress | null };

const ImportingView = ({ progress }: ImportingViewProps) => (
  <>
    <DialogHeader>
      <DialogTitle className="flex items-center gap-2">
        <YoutubeIcon className="size-5" />
        Importing from YouTube
      </DialogTitle>
      <DialogDescription>{progress?.message ?? 'Starting…'}</DialogDescription>
    </DialogHeader>
    <Progress value={progress?.percent ?? 0} />
    <DialogFooter>
      <p className="text-xs text-muted-foreground">
        You can close this dialog; the download continues in the background.
      </p>
    </DialogFooter>
  </>
);

type ErrorViewProps = { error: string; cookieStatus: YtCookieStatus | null; onRetry: () => void };

const ErrorView = ({ error, cookieStatus, onRetry }: ErrorViewProps) => {
  const dropPath = isCookieError(error) ? (cookieStatus?.dropPaths[0] ?? '') : '';
  return (
    <>
      <DialogHeader>
        <DialogTitle className="flex items-center gap-2">
          <AlertTriangleIcon className="size-5" />
          Import failed
        </DialogTitle>
        <DialogDescription>{error}</DialogDescription>
      </DialogHeader>
      {dropPath !== '' && (
        <p className="break-all text-xs text-muted-foreground">
          Fix: save your youtube.com cookies as {dropPath}
        </p>
      )}
      <DialogFooter>
        <Button onClick={onRetry}>Try again</Button>
      </DialogFooter>
    </>
  );
};

export const AddFromYoutubeDialog = () => {
  const { mode, close } = useDialog();
  const open = mode === 'add-from-youtube';
  const {
    flow,
    cookieStatus,
    runSearch,
    startImport,
    openConfirm,
    backToSearch,
    backFromConfirm,
    setSelectedLang,
    setQuery,
  } = useAddFromYoutubeFlow(open);

  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open && flow.view === 'search') {
      inputRef.current?.focus();
    }
  }, [open, flow.view]);

  return (
    <Dialog open={open} onOpenChange={(next) => !next && close()}>
      <DialogContent className="sm:max-w-lg">
        {flow.view === 'search' && (
          <SearchView
            flow={flow}
            cookieStatus={cookieStatus}
            inputRef={inputRef}
            onQueryChange={setQuery}
            onSearch={(query) => void runSearch(query)}
            onDirectImport={(input) => openConfirm(input)}
          />
        )}
        {flow.view === 'results' && (
          <ResultsView
            query={flow.query}
            results={flow.results}
            onPick={(id) => openConfirm(id)}
            onBack={backToSearch}
          />
        )}
        {flow.view === 'confirm' && (
          <ConfirmView
            input={flow.confirmInput}
            langs={flow.langs}
            selectedLang={flow.selectedLang}
            onLangChange={setSelectedLang}
            onImport={() => {
              void startImport(
                flow.confirmInput,
                flow.selectedLang === 'auto' ? undefined : flow.selectedLang,
              );
            }}
            onBack={backFromConfirm}
          />
        )}
        {flow.view === 'importing' && <ImportingView progress={flow.progress} />}
        {flow.view === 'error' && (
          <ErrorView error={flow.error} cookieStatus={cookieStatus} onRetry={backToSearch} />
        )}
      </DialogContent>
    </Dialog>
  );
};
