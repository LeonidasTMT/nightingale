//! YouTube import via yt-dlp.
//!
//! yt-dlp is installed into the vendored venv and run through the vendored
//! python, so no system dependencies are required. Search results come from
//! `ytsearchN:<query>` with a flat playlist; imports download best audio and
//! re-encode to mp3 into the configured library folder, which the regular
//! scanner then picks up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use ts_rs::TS;

use crate::{
    cache::CacheDir,
    config::{AppConfig, LibrarySource},
    library_db::load_all_songs,
    lyrics::{provide_lrc, write_lyrics_file},
    scanner::start_scan,
    song::{Song, SongOrigin},
    vendor::{analyzer_dir, ffmpeg_path, python_path, silent_command, uv_path},
};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum YtImportPhase {
    Searching,
    Downloading,
    Processing,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct YtImportProgress {
    pub phase: YtImportPhase,
    /// 0..=100 progress within the current phase.
    pub percent: u8,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct YtSearchResult {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uploader: Option<String>,
}

/// Folder the library scans. Imports must land here so the scanner registers
/// them; remote sources (Jellyfin/Plex/…) can't accept local files.
fn library_folder_root() -> Result<PathBuf, String> {
    let config = AppConfig::load();
    match config.library_source.as_ref() {
        Some(LibrarySource::Folder { path }) => Ok(path.clone()),
        _ => Err(
            "YouTube import needs a local folder library. Point the library at a \
             folder first (Settings → Library)."
                .to_string(),
        ),
    }
}

/// Auth args for yt-dlp so requests carry a real user session and can
/// bypass "Sign in to confirm you're not a bot" rate limiting.
/// Resolution order:
/// 1. Netscape-format cookie file: `$NIGHTINGALE_YT_COOKIES`,
///    `<data dir>/yt_cookies.txt`, `<library folder>/yt_cookies.txt` (the
///    last one is handy in Docker where only the music folder is
///    bind-mounted). An explicit file always wins — operator control.
/// 2. Auto: live cookies from an installed browser (`--cookies-from-browser`),
///    preferring a browser whose cookie store already contains youtube.com
///    entries (i.e. a logged-in session). `$NIGHTINGALE_YT_BROWSER` forces a
///    browser name (optionally `name:profile`) or disables this step with
///    `off`/`none`.
///
/// Returns an empty vec when nothing applies — unauthenticated requests
/// still work for many videos.
fn yt_cookies_args() -> Vec<String> {
    if let Some(args) = cookie_file_args() {
        return args;
    }
    match detect_cookie_browser() {
        Some(browser) => vec!["--cookies-from-browser".to_string(), browser],
        None => Vec::new(),
    }
}

/// `yt_cookies.txt` locations checked in priority order.
fn cookie_file_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(custom) = std::env::var("NIGHTINGALE_YT_COOKIES")
        .ok()
        .filter(|v| !v.is_empty())
    {
        candidates.push(PathBuf::from(custom));
    }
    candidates.push(
        AppConfig::load()
            .effective_data_path()
            .join("yt_cookies.txt"),
    );
    if let Ok(root) = library_folder_root() {
        candidates.push(root.join("yt_cookies.txt"));
    }
    candidates
}

fn cookie_file_args() -> Option<Vec<String>> {
    cookie_file_candidates()
        .into_iter()
        .find(|p| p.is_file())
        .map(|path| vec!["--cookies".to_string(), path.to_string_lossy().into_owned()])
}

/// What the next import will use for YouTube sign-in, plus where to drop a
/// cookie file. Surfaced in the Add-from-YouTube dialog so users see the
/// actual locations instead of "the data folder or library folder".
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct YtCookieStatus {
    /// `"file:<path>"`, `"browser:<spec>"`, or None (no sign-in).
    pub active: Option<String>,
    /// Where to put `yt_cookies.txt`, in priority order.
    pub drop_paths: Vec<String>,
}

pub fn yt_cookie_status() -> YtCookieStatus {
    let candidates = cookie_file_candidates();
    let active = match candidates.iter().find(|p| p.is_file()) {
        Some(path) => Some(format!("file:{}", path.to_string_lossy())),
        None => detect_cookie_browser()
            .map(|browser| format!("browser:{}", browser)),
    };
    YtCookieStatus {
        active,
        drop_paths: candidates.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
    }
}

/// Browsers yt-dlp's `--cookies-from-browser` accepts (v2026.x).
const KNOWN_BROWSERS: [&str; 9] = [
    "brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale",
];

/// One installed browser's cookie stores, probed for a YouTube login.
struct BrowserProbe {
    name: &'static str,
    /// `(profile dir name, cookie DB path)` pairs found under this browser.
    cookie_files: Vec<(String, PathBuf)>,
    /// Profile whose DB mentions youtube.com (the logged-in one), if any.
    yt_profile: Option<String>,
}

impl BrowserProbe {
    fn spec(&self) -> String {
        match &self.yt_profile {
            Some(profile) => format!("{}:{}", self.name, profile),
            None => self.name.to_string(),
        }
    }
}

/// `--cookies-from-browser` value for the best available browser session,
/// or None when no usable browser is installed.
fn detect_cookie_browser() -> Option<String> {
    // Explicit operator override: a known browser name (optionally
    // `name:profile`) wins; `off`/`none` disables auto-detection.
    if let Ok(raw) = std::env::var("NIGHTINGALE_YT_BROWSER") {
        let v = raw.trim();
        match v.to_lowercase().as_str() {
            "" | "off" | "none" => return None,
            "auto" => {} // fall through to detection
            other => {
                let name = other.split_once(':').map(|(n, _)| n).unwrap_or(other);
                if KNOWN_BROWSERS.contains(&name) {
                    return Some(v.to_string());
                }
                warn!("[yt-import] unknown NIGHTINGALE_YT_BROWSER '{other}', ignoring override");
            }
        }
    }

    let probes = cookie_probes();
    // Prefer a browser that actually holds youtube.com cookies (logged-in
    // session); otherwise fall back to the first installed browser at all.
    probes
        .iter()
        .find(|p| p.yt_profile.is_some())
        .or_else(|| probes.iter().find(|p| !p.cookie_files.is_empty()))
        .map(BrowserProbe::spec)
}

/// Chromium-family cookie DBs under `<root>/<profile>/Cookies` or the newer
/// `<root>/<profile>/Network/Cookies`. Returns `(profile, path)` pairs.
fn chromium_cookie_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let profile_dir = entry.path();
        if !profile_dir.is_dir() {
            continue;
        }
        let Some(profile) = profile_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let network = profile_dir.join("Network/Cookies");
        let legacy = profile_dir.join("Cookies");
        let file = if network.is_file() {
            network
        } else if legacy.is_file() {
            legacy
        } else {
            continue;
        };
        out.push((profile.to_string(), file));
    }
    out
}

fn make_probe(name: &'static str, cookie_files: Vec<(String, PathBuf)>) -> Option<BrowserProbe> {
    if cookie_files.is_empty() {
        return None;
    }
    let yt_profile = cookie_files
        .iter()
        .find(|(_, file)| file_contains(file, b"youtube.com"))
        .map(|(profile, _)| profile.clone());
    Some(BrowserProbe {
        name,
        yt_profile,
        cookie_files,
    })
}

/// All installed browsers worth probing, in preference order. Only browsers
/// with at least one readable cookie DB are included.
fn cookie_probes() -> Vec<BrowserProbe> {
    let mut probes: Vec<BrowserProbe> = Vec::new();
    let mut push = |name: &'static str, files: Vec<(String, PathBuf)>| {
        if let Some(probe) = make_probe(name, files) {
            probes.push(probe);
        }
    };

    #[cfg(windows)]
    {
        if let (Ok(local), Ok(home)) = (std::env::var("LOCALAPPDATA"), std::env::var("USERPROFILE"))
        {
            for (name, vendor) in [
                ("edge", PathBuf::from("Microsoft/Edge")),
                ("chrome", PathBuf::from("Google/Chrome")),
                ("brave", PathBuf::from("BraveSoftware/Brave-Browser")),
                ("chromium", PathBuf::from("Chromium")),
            ] {
                let root = Path::new(&local).join(vendor).join("User Data");
                push(name, chromium_cookie_files(&root));
            }
            // Opera keeps profiles under `user_windows`; very old installs use
            // a single `Opera Stable` profile dir (cookie DB at its root).
            let mut opera = chromium_cookie_files(
                &Path::new(&home).join("AppData/Roaming/Opera Software/user_windows"),
            );
            if opera.is_empty() {
                let stable = Path::new(&home).join("AppData/Roaming/Opera Software/Opera Stable");
                for rel in ["Network/Cookies", "Cookies"] {
                    let file = stable.join(rel);
                    if file.is_file() {
                        opera.push(("Default".to_string(), file));
                        break;
                    }
                }
            }
            push("opera", opera);

            // Firefox: profile dirs are hashed (`<hash>.default…`), DB is
            // `cookies.sqlite`. Low priority — Chromium sessions decrypt more
            // reliably — but cheap to include.
            let profiles = Path::new(&home).join("AppData/Roaming/Mozilla/Firefox/Profiles");
            let mut firefox: Vec<(String, PathBuf)> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(profiles) {
                for entry in entries.flatten() {
                    let dir = entry.path();
                    let file_name = entry.file_name();
                    let Some(dir_name) = file_name.to_str() else {
                        continue;
                    };
                    if !dir_name.contains(".default") {
                        continue;
                    }
                    let file = dir.join("cookies.sqlite");
                    if file.is_file() {
                        firefox.push((dir_name.to_string(), file));
                    }
                }
            }
            push("firefox", firefox);
        }
    }

    #[cfg(not(windows))]
    {
        // Best-effort for native Linux/macOS desktops: same
        // `<root>/<profile>/Cookies` layout under the per-OS config dir.
        // (Inside Docker there is no browser, so this finds nothing.)
        if let Ok(home) = std::env::var("HOME") {
            #[cfg(target_os = "macos")]
            let base = Path::new(&home).join("Library/Application Support");
            #[cfg(not(target_os = "macos"))]
            let base = Path::new(&home).join(".config");
            for (name, dir) in [
                (
                    "edge",
                    PathBuf::from(if cfg!(target_os = "macos") {
                        "Microsoft Edge"
                    } else {
                        "microsoft-edge"
                    }),
                ),
                (
                    "chrome",
                    PathBuf::from(if cfg!(target_os = "macos") {
                        "Google/Chrome"
                    } else {
                        "google-chrome"
                    }),
                ),
                (
                    "brave",
                    PathBuf::from(if cfg!(target_os = "macos") {
                        "BraveSoftware/Brave Browser"
                    } else {
                        "brave-browser"
                    }),
                ),
                ("chromium", PathBuf::from("Chromium")),
                ("vivaldi", PathBuf::from("Vivaldi")),
                ("opera", PathBuf::from("opera")),
            ] {
                push(name, chromium_cookie_files(&base.join(dir)));
            }
        }
    }

    probes
}

/// Bounded whole-file substring check. Cookie DBs are a few MB at most; the
/// cap keeps a pathological profile from loading unbounded data.
fn file_contains(path: &Path, needle: &[u8]) -> bool {
    const CAP: u64 = 64 * 1024 * 1024;
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() > CAP {
        return false;
    }
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    data.windows(needle.len()).any(|window| window == needle)
}

/// yt-dlp is not part of the base venv install list; make sure it exists so
/// installs from before this fork keep working without a full re-setup.
fn ensure_yt_dlp() -> Result<(), String> {
    // Bounded like every other probe: a wedged interpreter must not hang the
    // request forever.
    match run_yt_dlp_capture(&["--version"], Duration::from_secs(60)) {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!("[yt-import] yt-dlp probe failed: {stderr}");
        }
        Err(e) => {
            warn!("[yt-import] failed to probe yt-dlp: {e}");
        }
    }

    let uv = uv_path();
    let py = python_path();
    let out = silent_command(&uv)
        .args(["pip", "install", "yt-dlp", "--python"])
        .arg(&py)
        .output()
        .map_err(|e| format!("Failed to run uv for yt-dlp install: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("yt-dlp install failed: {stderr}"));
    }

    Ok(())
}

/// Fetch yt-dlp's info JSON for a video URL. Best-effort: `None` on any
/// failure so callers can fall back gracefully.
fn fetch_video_info(url: &str) -> Option<Value> {
    let mut args: Vec<String> = vec!["--skip-download".into(), "--no-playlist".into()];
    for arg in yt_cookies_args() {
        args.push(arg);
    }
    args.push("-j".into());
    args.push(url.to_string());
    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = run_yt_dlp_capture(&args_refs, Duration::from_secs(300)).ok()?;
    if !output.status.success() {
        return None;
    }
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("fetch_video_info: bad info json: {e}");
            None
        }
    }
}

/// Pick the caption language that matches the audio itself.
///
/// YouTube serves machine-translated auto-captions alongside the original
/// track, and in current yt-dlp output they are indistinguishable (no `tlang`
/// marker) with arbitrary track order — so the first track is often a
/// translation (e.g. Chinese for a Japanese song). We therefore derive the
/// expected spoken language from the video itself (title/description script,
/// then channel country) and prefer a matching track; only when no signal
/// exists do we fall back to the first auto-caption / manual subtitle.
fn pick_caption_language(url: &str) -> Option<String> {
    let info = fetch_video_info(url)?;

    // 0) A track matching the expected spoken language (auto first, then
    // manual). Exact match before primary-subtag prefix (ja ~ ja-JP).
    if let Some(exp) = expected_caption_language(&info) {
        for src in [info.get("automatic_captions"), info.get("subtitles")] {
            if let Some(map) = src.and_then(|v| v.as_object()) {
                if map.contains_key(exp.as_str()) {
                    return Some(exp);
                }
                let exp_tag = primary_subtag(&exp);
                if let Some(lang) = map.keys().find(|k| k.split('-').next() == Some(exp_tag)) {
                    return Some(lang.clone());
                }
            }
        }
    }

    // 1) Original-language auto-caption track (its URL has no tlang= param).
    if let Some(ac) = info.get("automatic_captions").and_then(|v| v.as_object())
        && let Some((lang, _)) = ac.iter().find(|(_, tracks)| {
            tracks
                .as_array()
                .and_then(|a| a.first())
                .and_then(|t| t.get("url"))
                .and_then(|u| u.as_str())
                .map(|u| !u.contains("tlang="))
                .unwrap_or(false)
        })
    {
        return Some(lang.clone());
    }

    // 2) Any manual subtitle.
    info.get("subtitles")
        .and_then(|v| v.as_object())
        .and_then(|m| m.keys().next().cloned())
}

/// The language the audio is actually sung/spoken in, derived from video
/// metadata. Kana/hangul in the title or description are unambiguous script
/// signals; the channel's registered country is a weaker proxy (many CJK
/// artists register in the US), so it only runs when no script was found.
fn expected_caption_language(info: &Value) -> Option<String> {
    let mut text = String::new();
    if let Some(t) = info.get("title").and_then(|v| v.as_str()) {
        text.push_str(t);
    }
    if let Some(d) = info.get("description").and_then(|v| v.as_str()) {
        text.push(' ');
        text.push_str(d);
    }

    let kana = text
        .chars()
        .filter(|c| matches!(c, '\u{3040}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}'))
        .count();
    let hangul = text
        .chars()
        .filter(|c| matches!(c, '\u{ac00}'..='\u{d7a3}' | '\u{1100}'..='\u{11ff}'))
        .count();
    if kana > 0 || hangul > 0 {
        return Some(if hangul > kana { "ko" } else { "ja" }.to_string());
    }

    let channel_id = info.get("channel_id")?.as_str()?;
    let country = channel_country(channel_id)?;
    language_for_country(&country)
}

fn primary_subtag(lang: &str) -> &str {
    lang.split('-').next().unwrap_or(lang)
}

/// Map a YouTube about-page country name to the expected CJK language.
/// Non-CJK countries return None so the old behavior applies.
fn language_for_country(country: &str) -> Option<String> {
    let lang = match country.trim() {
        "Japan" => "ja",
        "South Korea" | "North Korea" => "ko",
        "China" => "zh-Hans",
        "Taiwan" | "Hong Kong" | "Macau" => "zh-Hant",
        _ => return None,
    };
    Some(lang.to_string())
}

/// Best-effort fetch of the channel's registered country from its about
/// page. Any failure just means "no signal".
fn channel_country(channel_id: &str) -> Option<String> {
    const SCRIPT: &str = r#"
import re, sys, urllib.request
req = urllib.request.Request(
    "https://www.youtube.com/channel/" + sys.argv[1] + "/about?hl=en",
    headers={"User-Agent": "Mozilla/5.0", "Accept-Language": "en-US,en;q=0.9"},
)
html = urllib.request.urlopen(req, timeout=20).read().decode("utf-8", "replace")
m = re.search(r'\"country\":\"([^\"]+)\"', html)
print(m.group(1) if m else "")
"#;
    let python = python_path();
    if !python.exists() {
        return None;
    }
    let child = match silent_command(&python)
        .arg("-c")
        .arg(SCRIPT)
        .arg(channel_id)
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[yt-import] channel-country helper spawn failed: {e}");
            return None;
        }
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(o) => o,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_process_tree(pid);
            warn!("[yt-import] channel-country helper timed out");
            return None;
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return None,
    };
    let Ok(output) = output else { return None };
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let country = stdout.trim().to_string();
    (!country.is_empty()).then_some(country)
}

/// Run a short yt-dlp invocation and capture its output, bounded by a timeout.
fn run_yt_dlp_capture(args: &[&str], timeout: Duration) -> Result<std::process::Output, String> {
    let mut cmd = silent_command(python_path());
    // yt-dlp shells out to ffmpeg (subtitle conversion at least); make sure
    // it finds the vendored one, same as the import does.
    let ffmpeg = ffmpeg_path();
    if let (Some(parent), Some(existing)) = (ffmpeg.parent(), std::env::var_os("PATH")) {
        let mut paths = std::env::split_paths(&existing).collect::<Vec<_>>();
        paths.insert(0, parent.to_path_buf());
        cmd.env("PATH", std::env::join_paths(paths).unwrap_or(existing));
    }
    cmd.arg("-m")
        .arg("yt_dlp")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start yt-dlp: {e}"))?;
    let pid = child.id();

    // wait_with_output drains both pipes concurrently, so a full buffer can't
    // deadlock us; the channel lets us enforce the deadline from here.
    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(format!("yt-dlp failed: {e}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_process_tree(pid);
            Err(format!("yt-dlp timed out after {}s", timeout.as_secs()))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("yt-dlp exited unexpectedly".to_string()),
    }
}

/// Kill the process and its children (yt-dlp may have spawned ffmpeg).
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut cmd = Command::new("taskkill");
        cmd.creation_flags(CREATE_NO_WINDOW)
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.status();
    }

    #[cfg(not(windows))]
    {
        // Same pattern the analyzer server uses for its own shutdown path.
        let mut cmd = Command::new("kill");
        cmd.arg("-9").arg(pid.to_string());
        let _ = cmd.status();
    }
}

/// Search YouTube and return up to ~10 flat results (no per-video fetch).
pub fn search_youtube(query: &str) -> Result<Vec<YtSearchResult>, String> {
    ensure_yt_dlp()?;

    let query = query.trim();
    if query.is_empty() {
        return Err("Empty search query".to_string());
    }

    let spec = format!("ytsearch10:{query}");
    let mut args: Vec<String> = vec![
        spec,
        "--flat-playlist".to_string(),
        "--skip-download".to_string(),
        "--no-playlist".to_string(),
        "-j".to_string(),
        "--no-warnings".to_string(),
    ];
    args.extend(yt_cookies_args());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let out = run_yt_dlp_capture(&args, SEARCH_TIMEOUT)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let start = lines.len().saturating_sub(5);
        return Err(format!(
            "YouTube search failed: {}",
            lines[start..].join(" | ")
        ));
    }

    let stdout_text = String::from_utf8_lossy(&out.stdout);
    let mut results = Vec::new();
    for raw_line in stdout_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        results.push(YtSearchResult {
            id: id.to_string(),
            title: value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            duration_sec: value
                .get("duration")
                .and_then(Value::as_u64)
                .map(|d| d.min(u32::MAX as u64) as u32),
            uploader: value
                .get("channel")
                .or_else(|| value.get("uploader"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }

    Ok(results)
}

/// Download one video as mp3 into the library folder and rescan so the new
/// file shows up in the library. Returns the final mp3 path.
/// `caption_lang` pins the caption track that becomes the lyrics; `None`
/// lets the importer pick the language that matches the audio.
pub fn import_youtube(
    input: &str,
    caption_lang: Option<&str>,
    on_progress: impl FnMut(YtImportProgress),
) -> Result<PathBuf, String> {
    match import_youtube_impl(input, caption_lang, on_progress) {
        Ok(path) => Ok(path),
        Err(error) => {
            // The error also reaches the client as a `yt-import-error` event;
            // this is for self-hosted debugging where there is no UI log.
            tracing::error!("[yt-import] import failed: {error}");
            Err(error)
        }
    }
}

fn import_youtube_impl(
    input: &str,
    requested_caption: Option<&str>,
    mut on_progress: impl FnMut(YtImportProgress),
) -> Result<PathBuf, String> {
    ensure_yt_dlp()?;

    let root = library_folder_root()?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let input = input.trim();
    let (url, video_id) = if input.starts_with("http") {
        (input.to_string(), extract_video_id(input))
    } else {
        (
            format!("https://www.youtube.com/watch?v={input}"),
            Some(input.to_string()),
        )
    };

    // Snapshot existing mp3s so we can tell what the download produced even
    // when yt-dlp reuses a deterministic output name.
    let before = list_mp3s(&root);

    let mut cmd = silent_command(python_path());
    cmd.arg("-m")
        .arg("yt_dlp")
        .arg(&url)
        .arg("--no-playlist")
        .arg("-f")
        .arg("bestaudio/best")
        .arg("-x")
        .arg("--audio-format")
        .arg("mp3")
        .arg("-o")
        .arg(format!("{}/%(title)s [%(id)s].%(ext)s", root.display()))
        // Embed ID3 tags (title/artist from the video metadata) and the
        // thumbnail as cover art so the scanner fills in album art too.
        .arg("--embed-metadata")
        .arg("--embed-thumbnail")
        // Subtitles become lyrics: manual subs if present, else auto-captions,
        // converted to SRT which we turn into an LRC sidecar post-download.
        .arg("--write-subs")
        .arg("--write-auto-subs")
        .arg("--convert-subs")
        .arg("srt")
        .arg("--progress")
        .arg("--no-warnings");

    for arg in yt_cookies_args() {
        cmd.arg(arg);
    }

    // Pin the caption track. An explicit user choice wins; otherwise pick the
    // language that matches the audio. yt-dlp's own default prefers English,
    // which for non-English songs means a machine translation of the lyrics -
    // useless for karaoke.
    let caption_lang = match requested_caption.map(str::trim).filter(|l| !l.is_empty()) {
        Some(lang) => Some(lang.to_string()),
        None => pick_caption_language(&url),
    };
    if let Some(ref lang) = caption_lang {
        tracing::info!(lang = %lang, "yt-import: using caption language");
        cmd.arg("--sub-langs").arg(lang.as_str());
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // yt-dlp shells out to ffmpeg for the mp3 conversion; make sure it finds
    // the vendored one, same as the analyzer server does.
    let ffmpeg = ffmpeg_path();
    if let (Some(parent), Some(existing)) = (ffmpeg.parent(), std::env::var_os("PATH")) {
        let mut paths = std::env::split_paths(&existing).collect::<Vec<_>>();
        paths.insert(0, parent.to_path_buf());
        cmd.env("PATH", std::env::join_paths(paths).unwrap_or(existing));
    }

    on_progress(YtImportProgress {
        phase: YtImportPhase::Downloading,
        percent: 0,
        message: "Starting download…".to_string(),
    });

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start yt-dlp: {e}"))?;

    let result = run_import_child(child, &root, &before, video_id.as_deref(), &mut on_progress);

    // Register the new file with the library.
    if let Ok(path) = &result {
        remove_intermediate_audio(path);
        start_scan();
        // Subtitle -> LRC handoff runs off the import thread so this command
        // returns promptly; it waits for the scanner's row on its own.
        let path = path.clone();
        std::thread::spawn(move || attach_subtitles_as_lyrics(&path, caption_lang.as_deref()));
    }

    result
}

/// A caption track available for a video, as reported by yt-dlp.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct YtSubLanguage {
    /// Language code as YouTube reports it, e.g. `ja`, `ko`, `en-US`.
    pub lang: String,
    /// Where the track comes from: `"manual"` (uploaded) or `"auto"`
    /// (machine-generated).
    pub source: String,
}

/// List the caption tracks available for a video so the user can pick which
/// one becomes the lyrics. `input` is a URL or bare video id.
pub fn youtube_subtitle_languages(input: &str) -> Result<Vec<YtSubLanguage>, String> {
    let input = input.trim();
    let is_bare_id = input.len() == 11
        && input
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let url = if input.starts_with("http") {
        input.to_string()
    } else if is_bare_id {
        format!("https://www.youtube.com/watch?v={input}")
    } else {
        return Err("Expected a YouTube link or video id".to_string());
    };

    let info = fetch_video_info(&url)
        .ok_or_else(|| "Could not load the video's caption list (check the link)".to_string())?;

    let mut out: Vec<YtSubLanguage> = Vec::new();
    let mut seen = HashSet::new();
    if let Some(map) = info.get("subtitles").and_then(|v| v.as_object()) {
        for (lang, _) in map.iter() {
            if lang == "live_chat" || !seen.insert(lang.clone()) {
                continue;
            }
            out.push(YtSubLanguage {
                lang: lang.clone(),
                source: "manual".to_string(),
            });
        }
    }
    if let Some(map) = info.get("automatic_captions").and_then(|v| v.as_object()) {
        for (lang, _) in map.iter() {
            if !seen.insert(lang.clone()) {
                continue;
            }
            out.push(YtSubLanguage {
                lang: lang.clone(),
                source: "auto".to_string(),
            });
        }
    }
    Ok(out)
}

/// Extract a YouTube video id from a library path. Imported files are named
/// by yt-dlp's output template as `<title> [<id>].mp3`, and that bracketed id
/// is the only persisted record of where the song came from — the DB row
/// itself knows nothing about YouTube. Returns None for anything that does
/// not look like an import, so callers can fall back to their normal path.
pub(crate) fn video_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let mut start = 0;
    while let Some(rel) = name[start..].find('[') {
        let open = start + rel + 1;
        let Some(close_rel) = name[open..].find(']') else {
            break;
        };
        let close = open + close_rel;
        let candidate = &name[open..close];
        if candidate.len() == 11
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Some(candidate.to_string());
        }
        start = close + 1;
    }
    None
}

/// Re-pull the caption track of a song that was imported from YouTube and
/// rebuild its lyrics from it, returning a lyrics JSON path in exactly the
/// same shape as `fetch_lrclib_lyrics` — this is what "Refetch lyrics &
/// align" uses for such songs before falling back to LRCLIB. The caption
/// text also replaces the sidecar `.lrc` next to the audio (the editable
/// source of truth for the lyrics UI), with romanized readings attached as
/// at import time.
///
/// When a caption language was resolved and the row had none, `song.language`
/// is filled in so the worker gets a proper alignment hint. Returns None
/// when the song has no YouTube provenance in its filename or any step
/// fails; callers then keep their normal lyrics path.
pub(crate) fn fetch_yt_lyrics(song: &mut Song, cache: &CacheDir) -> Option<PathBuf> {
    let video_id = video_id_from_path(&song.path)?;
    let url = format!("https://www.youtube.com/watch?v={video_id}");

    // Same language choice as the original import: a known song language
    // wins, otherwise derive it from the video (script in title/description,
    // then channel country).
    let lang = match song.language.clone().filter(|l| !l.is_empty() && l.as_str() != "unknown") {
        Some(l) => Some(l),
        None => pick_caption_language(&url),
    };

    // Download just the caption track into a scratch dir (no audio). The dir
    // is wiped first so a .srt left over by an earlier failed run can never
    // be mistaken for this one.
    let tmp = std::env::temp_dir().join(format!("nightingale-yt-{}", song.file_hash));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).ok()?;

    let mut args: Vec<String> = vec![
        url,
        "--no-playlist".into(),
        "--skip-download".into(),
        "--write-subs".into(),
        "--write-auto-subs".into(),
        "--convert-subs".into(),
        "srt".into(),
        "-o".into(),
        format!("{}/cap", tmp.display()),
    ];
    if let Some(l) = &lang {
        args.push("--sub-langs".into());
        args.push(l.clone());
    }
    args.extend(yt_cookies_args());
    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let out = match run_yt_dlp_capture(&args_refs, Duration::from_secs(300)) {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("[yt-import] caption refetch failed for video {video_id}: {e}");
            return None;
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().collect();
        let start = tail.len().saturating_sub(3);
        tracing::warn!(
            "[yt-import] caption refetch failed for video {video_id}: {}",
            tail[start..].join(" | ")
        );
        return None;
    }

    // yt-dlp names the converted track `<stem>.<lang>.srt` next to -o.
    let srt_path = match std::fs::read_dir(&tmp)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|ext| ext == "srt"))
    {
        Some(p) => p,
        None => {
            tracing::warn!(
                "[yt-import] no caption track found for video {video_id} (language {lang:?})"
            );
            return None;
        }
    };

    let Ok(srt_text) = std::fs::read_to_string(&srt_path) else {
        tracing::warn!("[yt-import] could not read refetched captions: {}", srt_path.display());
        return None;
    };
    let Some(lrc_plain) = srt_to_lrc(&srt_text) else {
        tracing::warn!("[yt-import] refetched captions contained no usable lines for {video_id}");
        return None;
    };

    // Romanized readings under each line, exactly as at import time.
    let lrc_final = attach_readings(&lrc_plain, lang.as_deref());

    // Sidecar next to the song: the editable source of truth for the lyrics UI.
    if let (Some(parent), Some(stem)) = (song.path.parent(), song.path.file_stem()) {
        let _ = std::fs::write(
            parent.join(format!("{}.lrc", stem.to_string_lossy())),
            &lrc_final,
        );
    }

    // Plain sung lines for the worker, which aligns them to the audio.
    let lines: Vec<String> = lrc_plain
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let end = line.find(']')?;
            let text = line[end + 1..].trim();
            (!text.is_empty()).then_some(text.to_string())
        })
        .collect();
    if lines.len() < 2 {
        tracing::warn!("[yt-import] refetched captions produced too few lines for {video_id}");
        return None;
    }

    let Some(out_path) = write_lyrics_file(cache, &song.file_hash, &lines).ok() else {
        tracing::warn!("[yt-import] failed to write refetched lyrics for {video_id}");
        return None;
    };

    if song.language
        .as_ref()
        .filter(|l| !l.is_empty() && l.as_str() != "unknown")
        .is_none()
        && let Some(l) = lang
    {
        song.language = Some(l);
    }

    tracing::info!("[yt-import] refetched lyrics from captions for video {video_id}");
    Some(out_path)
}

/// Refresh title/artist/duration/cover for a YouTube-imported song straight
/// from the video, so "Refresh metadata" tracks upstream renames and new
/// cover art. Returns true when the song is a YouTube import and the fetch
/// succeeded (callers must not fall back to file tags); false otherwise, in
/// which case callers keep their normal path.
///
/// The mp3's own ID3 tags are deliberately left alone: rewriting them would
/// change the file hash and invalidate every analysis cache for the song.
pub(crate) fn refresh_yt_metadata(song: &mut Song, cache: &CacheDir) -> bool {
    if !matches!(song.origin, SongOrigin::LocalFile) {
        return false;
    }
    let Some(video_id) = video_id_from_path(&song.path) else {
        return false;
    };
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let info = match fetch_video_info(&url) {
        Some(v) => v,
        None => {
            tracing::warn!(
                "[yt-import] metadata refresh: could not load video {video_id}; keeping file tags"
            );
            return false;
        }
    };

    if let Some(title) = info.get("title").and_then(Value::as_str)
        && !title.is_empty()
    {
        song.title = title.to_string();
    }
    let artist = info
        .get("uploader")
        .or_else(|| info.get("channel"))
        .and_then(Value::as_str);
    if let Some(artist) = artist
        && !artist.is_empty()
    {
        song.artist = artist.to_string();
    }
    if let Some(duration) = info.get("duration").and_then(Value::as_f64)
        && duration > 0.0
    {
        song.duration_secs = duration;
    }
    if let Some(thumbnail) = info.get("thumbnail").and_then(Value::as_str) {
        match download_cover_bytes(thumbnail) {
            Ok(bytes) => {
                // Content-addressed like every other cover, so a refresh after
                // the cached file was deleted re-materializes it and an
                // unchanged thumbnail is a no-op write.
                let cover_hash = blake3::hash(&bytes).to_hex()[..32].to_string();
                let cover_path = cache.cover_path(&cover_hash);
                if !cover_path.exists() {
                    let _ = std::fs::write(&cover_path, &bytes);
                }
                song.album_art_path = Some(cover_path);
            }
            Err(e) => tracing::warn!("[yt-import] metadata refresh: cover download failed: {e}"),
        }
    }

    tracing::info!("[yt-import] metadata refreshed from video {video_id}");
    true
}

fn download_cover_bytes(url: &str) -> Result<Vec<u8>, String> {
    let agent = ureq::Agent::new_with_defaults();
    let resp = agent
        .get(url)
        .header("User-Agent", "Nightingale/1.0")
        .call()
        .map_err(|e| e.to_string())?;
    let bytes = resp.into_body().read_to_vec().map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("empty cover".to_string());
    }
    Ok(bytes)
}

/// Generate romanized readings for caption lines using the analyzer's CJK
/// helpers (pykakasi Hepburn for ja, Revised Romanization for ko, pinyin for
/// zh, Jyutping for yue). Returns one `Option` per line; `None` means no
/// reading (unsupported language, non-CJK text, or helper failure).
fn compute_readings(lang: &str, lines: &[String]) -> Vec<Option<String>> {
    const SCRIPT: &str = r#"
import sys, json, re
sys.path.insert(0, sys.argv[1])
from cjk import reading, _japanese_readings, tokenize_japanese
# Read stdin as raw bytes: the pipe is not a console, so sys.stdin would
# decode our UTF-8 payload with the Windows ANSI code page (e.g. cp932) and
# silently mangle CJK text.
raw = sys.stdin.buffer.read()
req = json.loads(raw.decode("utf-8", "replace"))
lang = req.get("lang") or ""
out = []
for line in req.get("lines") or []:
    r = None
    if lang == "ja" and re.search(r"[\u3040-\u30ff\u31f0-\u31ff\u4e00-\u9fff]", line):
        toks = tokenize_japanese(line)
        r = " ".join(x for x in _japanese_readings(toks, line) if x).strip() or None
    elif lang:
        r = " ".join(p for p in (reading(w, lang) or "" for w in line.split()) if p).strip() or None
    out.append(r)
print(json.dumps(out))
"#;
    let python = python_path();
    tracing::info!(
        path = %python.display(),
        exists = python.exists(),
        count = lines.len(),
        lang,
        pythonhome = ?std::env::var("PYTHONHOME"),
        "[yt-import] compute_readings entry"
    );
    if !python.exists() || lines.is_empty() {
        return Vec::new();
    }
    let payload = match serde_json::to_vec(&serde_json::json!({ "lang": lang, "lines": lines })) {
        Ok(p) => p,
        Err(e) => {
            warn!("[yt-import] reading helper payload failed: {e}");
            return Vec::new();
        }
    };
    use std::io::Write as _;
    let mut child = match Command::new(&python)
        .arg("-c")
        .arg(SCRIPT)
        .arg(analyzer_dir())
        // Force UTF-8 I/O so the payload survives any Windows ANSI code page.
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[yt-import] reading helper spawn failed: {e}");
            return Vec::new();
        }
    };
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&payload);
        let _ = stdin.flush();
    }
    // First run loads fugashi/pykakasi, which can take a while.
    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(o) => o,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_process_tree(pid);
            warn!("[yt-import] reading helper timed out");
            return Vec::new();
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            warn!("[yt-import] reading helper exited unexpectedly");
            return Vec::new();
        }
    };
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            warn!("[yt-import] reading helper wait failed: {e}");
            return Vec::new();
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "[yt-import] reading helper exited {}: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        );
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    tracing::info!(
        status_ok = output.status.success(),
        len = stdout.len(),
        head = %stdout.trim().chars().take(120).collect::<String>(),
        "[yt-import] reading helper output"
    );
    match serde_json::from_slice::<Vec<Option<String>>>(stdout.trim().as_bytes()) {
        Ok(readings) => readings,
        Err(e) => {
            warn!("[yt-import] reading helper output invalid: {e}");
            Vec::new()
        }
    }
}

/// Insert `;reading` lines (one per lyric line that has a reading) into the
/// LRC text so the readings live in the sidecar and flow through
/// `provide_lrc` into the transcript, like AI-transcribed CJK songs.
fn attach_readings(lrc_text: &str, lang: Option<&str>) -> String {
    let Some(lang) = lang else {
        return lrc_text.to_string();
    };
    // srt_to_lrc output is one `[mm:ss.xx] text` line per caption line.
    let texts: Vec<String> = lrc_text
        .lines()
        .map(|line| {
            line.split_once(']')
                .map(|(_, t)| t.trim().to_string())
                .unwrap_or_default()
        })
        .collect();
    let readings = compute_readings(lang, &texts);
    let mut out = String::new();
    for (i, line) in lrc_text.lines().enumerate() {
        out.push_str(line);
        out.push('\n');
        if !texts[i].is_empty()
            && let Some(Some(reading)) = readings.get(i)
        {
            out.push(';');
            out.push_str(reading);
            out.push('\n');
        }
    }
    out
}

/// Turn the subtitle sidecar yt-dlp wrote into an LRC file next to the song,
/// then hand it to the lyrics pipeline: the transcript is built straight from
/// the captions (no AI transcription) and a stems-only analysis pass is queued,
/// so the import is karaoke-ready as soon as separation finishes.
fn attach_subtitles_as_lyrics(mp3_path: &Path, caption_lang: Option<&str>) {
    let Some(srt_path) = find_subs_sidecar(mp3_path) else {
        return;
    };

    let Ok(srt_text) = std::fs::read_to_string(&srt_path) else {
        warn!(
            "[yt-import] could not read subtitles: {}",
            srt_path.display()
        );
        return;
    };

    let Some(lrc_text) = srt_to_lrc(&srt_text) else {
        let _ = std::fs::remove_file(&srt_path);
        warn!("[yt-import] subtitles contained no usable lines; skipped lyrics");
        return;
    };

    // Romanized readings under each line, like AI-transcribed CJK songs get.
    let lrc_text = attach_readings(&lrc_text, caption_lang);

    // Sidecar next to the song: the editable source of truth for the lyrics UI.
    if let (Some(parent), Some(stem)) = (mp3_path.parent(), mp3_path.file_stem()) {
        let lrc_path = parent.join(format!("{}.lrc", stem.to_string_lossy()));
        if std::fs::write(&lrc_path, &lrc_text).is_ok() {
            let _ = std::fs::remove_file(&srt_path);
        }
    }

    let Some(song) = wait_for_song(mp3_path) else {
        warn!("[yt-import] song never appeared in the library; lyrics not attached");
        return;
    };

    if song.is_analyzed {
        // Re-import of an already-analyzed song: keep the existing transcript and
        // stems, the sidecar .lrc stays available for editing.
        return;
    }

    match provide_lrc(&song.file_hash, &lrc_text, true) {
        Ok(()) => tracing::info!(
            "[yt-import] lyrics attached from captions: {}",
            mp3_path.display()
        ),
        Err(e) => warn!(
            "[yt-import] lyrics handoff failed for {}: {e}",
            mp3_path.display()
        ),
    }
}

/// The SRT yt-dlp wrote next to the audio file (`<stem>.<lang>.srt`).
fn find_subs_sidecar(mp3_path: &Path) -> Option<PathBuf> {
    let (Some(parent), Some(stem)) = (mp3_path.parent(), mp3_path.file_stem()) else {
        return None;
    };
    let stem = stem.to_string_lossy();
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("srt"))
            {
                return None;
            }
            let name = path.file_name()?.to_string_lossy();
            // `<stem>.<lang>.srt` -- must be longer than the stem plus ".".
            if name.starts_with(&stem[..]) && name.len() > stem.len() + 1 {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Poll the library until the scanner registers `mp3_path` (scan runs on its
/// own thread). Canonicalize both sides so Windows `\\?\` prefixes can't
/// break the comparison.
fn wait_for_song(mp3_path: &Path) -> Option<Song> {
    let canonical = std::fs::canonicalize(mp3_path).unwrap_or_else(|_| mp3_path.to_path_buf());
    for _ in 0..120 {
        if let Ok(songs) = load_all_songs()
            && let Some(song) = songs
                .into_iter()
                .find(|s| path_matches(&s.path, &canonical))
        {
            return Some(song);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

fn path_matches(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b
}

/// Convert an SRT sidecar into plain LRC. YouTube caption tracks repeat the
/// tail of each cue at the start of the next one, so consecutive duplicate
/// lines are dropped and empty cues skipped.
fn srt_to_lrc(srt_text: &str) -> Option<String> {
    let lines: Vec<&str> = srt_text.lines().collect();
    let mut out = String::new();
    let mut line_count = 0usize;
    let mut last_emitted: Option<String> = None;
    let mut last_ts_ms: i64 = -1;
    // Start of the cue currently being consumed, if any.
    let mut cue_start_ms: Option<i64> = None;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(start) = parse_srt_time_range(line).map(|(s, _)| s) {
            cue_start_ms = Some(start);
            i += 1;
            continue;
        }
        if line.is_empty() {
            // A blank right after the range (before the text) keeps the cue
            // open; a blank before the next index/range closes it.
            let mut peek = i + 1;
            while peek < lines.len() && lines[peek].trim().is_empty() {
                peek += 1;
            }
            if peek >= lines.len()
                || lines[peek].trim().parse::<u32>().is_ok()
                || parse_srt_time_range(lines[peek].trim()).is_some()
            {
                cue_start_ms = None;
            }
            i += 1;
            continue;
        }
        if line.parse::<u32>().is_ok() && cue_start_ms.is_none() {
            i += 1; // cue index outside any open cue
            continue;
        }
        if let Some(start) = cue_start_ms {
            let text = clean_subtitle_text(line);
            if !text.is_empty() && Some(&text) != last_emitted.as_ref() {
                let ts_ms = if start == last_ts_ms {
                    last_ts_ms + 10
                } else {
                    start
                };
                out.push_str(&format_lrc_timestamp(ts_ms));
                out.push(' ');
                out.push_str(&text);
                out.push('\n');
                line_count += 1;
                last_emitted = Some(text);
                last_ts_ms = ts_ms;
            }
        }
        i += 1;
    }

    if line_count >= 2 { Some(out) } else { None }
}

/// Parse `HH:MM:SS,mmm --> HH:MM:SS,mmm`, returning start/end in ms.
fn parse_srt_time_range(line: &str) -> Option<(i64, i64)> {
    let (left, right) = line.split_once("-->")?;
    match (parse_srt_timestamp(left), parse_srt_timestamp(right)) {
        (Some(start), Some(end)) => Some((start, end)),
        _ => None,
    }
}

fn parse_srt_timestamp(stamp: &str) -> Option<i64> {
    let stamp = stamp.trim();
    let (hms, frac) = match stamp.rfind([',', '.']) {
        Some(sep) => stamp.split_at(sep),
        None => (stamp, ""),
    };
    let mut parts = hms.split(':');
    let h = parts.next()?.trim().parse::<i64>().ok()?;
    let m = parts.next()?.trim().parse::<i64>().ok()?;
    let s = parts.next()?.trim().parse::<i64>().ok()?;
    let digits = frac.trim().chars().filter(|c| c.is_ascii_digit()).take(3);
    let mut ms: i64 = 0;
    for (idx, d) in digits.enumerate() {
        ms = ms * 10 + d.to_digit(10)? as i64;
        if idx == 2 {
            break;
        }
    }
    // Pad short fractions: "9" meant 900ms, "96" meant 960ms.
    let len = frac
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .count()
        .min(3);
    for _ in 0..(3 - len) {
        ms *= 10;
    }
    Some(((h * 60 + m) * 60 + s) * 1000 + ms)
}

/// Strip HTML tags/entities YouTube puts in caption lines and collapse runs of
/// whitespace.
fn clean_subtitle_text(line: &str) -> String {
    let mut without_tags = String::new();
    let mut in_tag = false;
    for ch in line.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => without_tags.push(ch),
            _ => {}
        }
    }
    let unescaped = without_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    unescaped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_lrc_timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    let min = ms / 60_000;
    let sec = (ms % 60_000) / 1000;
    let cs = (ms % 1000) / 10;
    format!("[{min:02}:{sec:02}.{cs:02}]")
}

/// yt-dlp usually deletes the intermediate audio file after conversion, but on
/// Windows it can be left behind (e.g. an AV lock during removal).
fn remove_intermediate_audio(mp3_path: &Path) {
    let Some((dir, stem)) = mp3_path.parent().zip(mp3_path.file_stem()) else {
        return;
    };
    for ext in ["webm", "m4a", "opus", "flac"] {
        let mut name = stem.to_os_string();
        name.push(format!(".{ext}"));
        let candidate = dir.join(name);
        if candidate.is_file() {
            let _ = std::fs::remove_file(candidate);
        }
    }
}

fn run_import_child(
    mut child: std::process::Child,
    root: &Path,
    before: &HashSet<PathBuf>,
    video_id: Option<&str>,
    on_progress: &mut impl FnMut(YtImportProgress),
) -> Result<PathBuf, String> {
    // Drain stdout in a background thread so the pipe never fills while we're
    // busy reading progress from stderr. (yt-dlp logs to stderr; with no
    // --print flags stdout stays empty, but don't rely on it.)
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while stdout.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
        });
    }

    let mut last_percent: u8 = 0;
    let mut tail: Vec<String> = Vec::new();

    if let Some(stderr) = child.stderr.take() {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else {
                break;
            };
            tail.push(line.clone());
            if tail.len() > 10 {
                tail.remove(0);
            }

            if line.contains("[ExtractAudio]") || line.contains("Converting") {
                on_progress(YtImportProgress {
                    phase: YtImportPhase::Processing,
                    percent: 0,
                    message: "Converting to mp3…".to_string(),
                });
            } else if let Some(percent) = parse_download_percent(&line)
                && percent != last_percent
            {
                last_percent = percent;
                on_progress(YtImportProgress {
                    phase: YtImportPhase::Downloading,
                    percent,
                    message: line.trim().to_string(),
                });
            }
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;

    if !status.success() {
        return Err(format!(
            "yt-dlp exited with {}: {}",
            status,
            tail.join(" | ")
        ));
    }

    // Find the new mp3 (or the pre-existing one when yt-dlp skipped a file it
    // already downloaded).
    let after = list_mp3s(root);
    let mut fresh: Vec<&PathBuf> = after.iter().filter(|p| !before.contains(*p)).collect();

    if fresh.is_empty() {
        let existing = video_id.and_then(|id| {
            after.iter().find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(&format!("[{id}]")))
            })
        });
        return match existing {
            Some(path) => Ok(path.clone()),
            None => Err("yt-dlp finished but no mp3 appeared in the library folder".to_string()),
        };
    }

    // Prefer the most recently modified file when several matched.
    fresh.sort_by_key(|p| mtime(p));
    Ok(fresh
        .pop()
        .expect("unique path must exist after the freshness loop")
        .clone())
}

/// Pulls the percent value out of a yt-dlp `[download]` progress line such as
/// `[download]   45.2% of 10.23MiB at …`.
fn parse_download_percent(line: &str) -> Option<u8> {
    if !line.starts_with("[download]") {
        return None;
    }
    let percent_pos = line.find('%')?;
    let before = &line[..percent_pos];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_ascii_digit() && *c != '.')
        .map(|(i, _)| i + 1)
        .unwrap_or(0);
    let num = &before[start..];
    if num.is_empty() {
        return None;
    }
    let (int_part, dec_part) = match num.find('.') {
        Some(pos) => (&num[..pos], &num[pos + 1..]),
        None => (num, ""),
    };
    let mut value: f64 = int_part.parse::<f64>().ok()?;
    if !dec_part.is_empty() {
        value += dec_part.parse::<f64>().ok()? / 10.0_f64.powi(dec_part.len() as i32);
    }
    Some(value.clamp(0.0, 100.0) as u8)
}

fn list_mp3s(root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "mp3") {
            out.insert(path);
        }
    }
    out
}

fn mtime(path: &Path) -> std::time::SystemTime {
    path.metadata()
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Extract the video id from a watch URL for the "already imported" lookup.
fn extract_video_id(url: &str) -> Option<String> {
    let after = url.split("v=").nth(1)?;
    Some(after.split(&['&', '?'][..]).next()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SRT: &str = "\
1
00:00:09,960 --> 00:00:14,030

Oh-ban, Gangnam Style,

2
00:00:14,030 --> 00:00:14,040


 
3
00:00:14,040 --> 00:00:18,670

Gangnam Style. A

4
00:00:18,670 --> 00:00:18,680


 
5
00:00:18,680 --> 00:00:22,670

humane woman who comes separately during the day, a

6
00:00:22,670 --> 00:00:22,680
humane woman who comes separately during the day, a
 
7
00:00:22,680 --> 00:00:25,710
humane woman who comes separately during the day, a
woman of class who knows the leisure of a cup of coffee, a woman whose

8
00:00:25,710 --> 00:00:25,720
woman of class who knows the leisure of a cup of coffee, a woman whose
";

    #[test]
    fn srt_to_lrc_dedupes_rolling_captions() {
        let lrc = srt_to_lrc(SAMPLE_SRT).expect("should produce lines");
        assert_eq!(
            lrc,
            "[00:09.96] Oh-ban, Gangnam Style,\n\
             [00:14.04] Gangnam Style. A\n\
             [00:18.68] humane woman who comes separately during the day, a\n\
             [00:22.68] woman of class who knows the leisure of a cup of coffee, a woman whose\n"
        );
    }

    #[test]
    fn srt_to_lrc_rejects_empty_input() {
        assert!(srt_to_lrc("1\n00:00:01,000 --> 00:00:02,000\n\nhello\n").is_none());
        assert!(srt_to_lrc("").is_none());
    }

    #[test]
    fn srt_timestamp_parsing() {
        assert_eq!(
            parse_srt_time_range("00:01:02,500 --> 00:01:05,000"),
            Some((62_500, 65_000))
        );
        assert_eq!(
            parse_srt_time_range("01:02:03,45 --> 01:02:04.9"),
            Some((3_723_450, 3_724_900))
        );
    }

    #[test]
    fn lrc_timestamp_format() {
        assert_eq!(format_lrc_timestamp(62_500), "[01:02.50]");
        assert_eq!(format_lrc_timestamp(3_724_900), "[62:04.90]");
    }

    #[test]
    fn clean_subtitle_text_strips_tags_and_entities() {
        assert_eq!(
            clean_subtitle_text("<i>hello &amp; <font>world</font></i>"),
            "hello & world"
        );
    }

    #[test]
    fn compute_readings_romanizes_korean_lines() {
        let lines = vec![
            "오 오".to_string(),
            "[음악]".to_string(),
            "hp so i'd rather".to_string(),
        ];
        let out = compute_readings("ko", &lines);
        assert_eq!(out.len(), 3);
        assert!(out[0].is_some());
        assert!(out[2].is_none());
    }

    #[test]
    fn video_id_from_path_finds_import_name() {
        let p = Path::new("C:\\songs\\Artist - Song [dQw4w9WgXcQ].mp3");
        assert_eq!(video_id_from_path(p), Some("dQw4w9WgXcQ".to_string()));
    }

    #[test]
    fn video_id_from_path_accepts_dash_and_underscore() {
        let p = Path::new("a - b [abcDEF-_123].mp3");
        assert_eq!(video_id_from_path(p), Some("abcDEF-_123".to_string()));
    }

    #[test]
    fn video_id_from_path_rejects_non_imports() {
        assert_eq!(video_id_from_path(Path::new("song.mp3")), None);
        // Too short / too long bracketed ids are not YouTube ids.
        assert_eq!(video_id_from_path(Path::new("song [short].mp3")), None);
        assert_eq!(
            video_id_from_path(Path::new("song [1234567890ab].mp3")),
            None
        );
    }
}
