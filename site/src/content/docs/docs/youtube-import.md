---
title: Import From YouTube
---

Nightingale can pull songs in from YouTube. Open **Add from YouTube** in the sidebar (visible while a folder library is active), then either search for a song or paste a video link / ID directly. Picking a result opens a confirm screen where you choose the caption track, and **Import** downloads the song into your library.

## How it works

- Uses the vendored Python + yt-dlp runtime — nothing extra to install.
- Downloads the best available audio and converts it to MP3 with metadata and cover art embedded (title from the video title, artist auto-filled from the uploader's channel name).
- The MP3 lands in your library folder and is scanned in like any other file, then queued for stem separation and analysis.
- A progress bar shows download/convert stages. You can close the dialog; the import continues in the background and the song appears when it finishes.

## Captions become lyrics

The confirm screen has a **Captions (used as lyrics)** dropdown:

- **Auto — match the song language** picks the caption track that matches the video's own language (e.g. a Japanese upload uses its Japanese captions).
- Otherwise pick a specific track; each is labeled `language (uploaded)` for human-uploaded captions or `language (machine)` for auto-generated ones.

The chosen track is attached as word-timed lyrics, so the song needs no ASR pass — and CJK tracks get romanized readings under every line (Japanese romaji, Korean Revised Romanization), the same reading system Nightingale uses for transcribed songs. If a video has no captions the import still works; you can add lyrics afterwards via **Provide LRC** / **Edit lyrics** ([Lyrics & Transcription](/docs/lyrics/)).

## Signing in with YouTube cookies

Many videos work without any sign-in, but YouTube asks *"Sign in to confirm you're not a bot"* for server and datacenter addresses (including most self-hosted/Docker setups) or after heavy use, and age-restricted videos require a logged-in session. When that happens, the import dialog tells you to add cookies.

Nightingale passes a real user session to yt-dlp so requests behave like a normal browser. It resolves credentials in this order:

1. **A Netscape-format cookie file** — the first of these that exists wins, and an explicit file always takes priority:
   - `NIGHTINGALE_YT_COOKIES` environment variable pointing at any cookie file,
   - `yt_cookies.txt` in your data folder,
   - `yt_cookies.txt` in your library (music) folder.
2. **An installed browser (automatic)** — on a machine with a desktop, Nightingale reads live cookies from an installed browser, preferring a profile that is already logged into YouTube. Set `NIGHTINGALE_YT_BROWSER` to force one (`chrome`, `edge`, optionally `name:profile`) or disable this step entirely (`off`).
3. **Nothing** — unauthenticated requests, which still work for many videos.

### Where do I put `yt_cookies.txt`?

The easiest place is your **library folder** — the music folder you picked during setup (folder libraries only; with Plex/Jellyfin/Navidrome sources use the data folder or the environment variable). It's a folder you already know:

| Setup | Library folder (drop the file here) | Data folder (alternative) |
| --- | --- | --- |
| Windows self-host (`run.bat`) | your music folder | `C:\Users\<you>\.nightingale-fork` |
| Any other install | your music folder | `%USERPROFILE%\.nightingale` (Windows) · `~/.nightingale` (Linux/macOS) |
| Docker | `/songs` — the mounted music folder | `/data` — the data volume |

No restart needed; each import picks the file up fresh.

### Creating `yt_cookies.txt`

1. Sign in to YouTube in your browser (the account you want imports to run as).
2. Export that browser's `youtube.com` cookies in Netscape format — e.g. with the "Get cookies.txt LOCALLY" extension, filtered to `youtube.com`.
3. Save the export as `yt_cookies.txt` in one of the locations above.

### Docker

Mount the cookie file into the container, e.g.:

```sh
docker run -d --name nightingale \
  -v /path/to/music:/songs:ro \
  -v /path/to/yt_cookies.txt:/data/yt_cookies.txt:ro \
  ...
```

or set `NIGHTINGALE_YT_COOKIES=/data/yt_cookies.txt` explicitly.

### Security notes

- A cookie file is a full session for that Google account — anyone holding it can act as you on YouTube (and on any other service present in the export). Keep it private and never commit it to git.
- Nightingale only reads the file locally and hands it to yt-dlp; it is never uploaded, synced, or stored anywhere else.
- If you'd rather not expose your main account, use a dedicated throwaway Google account for imports.

## Limits

- Audio is the best available YouTube rendition converted to MP3 — not original studio quality.
- Age-restricted videos need cookies **and** an account that passes YouTube's age gate.
- DRM'd or otherwise unavailable content cannot be imported.
- Videos without captions import fine, but arrive without lyrics until you add them manually.
