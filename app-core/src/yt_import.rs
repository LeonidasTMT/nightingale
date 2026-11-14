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
    config::{AppConfig, LibrarySource},
    scanner::start_scan,
    vendor::{ffmpeg_path, python_path, silent_command, uv_path},
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

fn cookie_file_args() -> Option<Vec<String>> {
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
        .into_iter()
        .find(|p| p.is_file())
        .map(|path| vec!["--cookies".to_string(), path.to_string_lossy().into_owned()])
}

/// Browsers yt-dlp's `--cookies-from-browser` accepts (v2026.x).
const KNOWN_BROWSERS: [&str; 9] = [
    "brave",
    "chrome",
    "chromium",
    "edge",
    "firefox",
    "opera",
    "safari",
    "vivaldi",
    "whale",
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
    probes.iter().find(|p| p.yt_profile.is_some()).or_else(|| {
        probes
            .iter()
            .find(|p| !p.cookie_files.is_empty())
    })
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
        if let (Ok(local), Ok(home)) = (
            std::env::var("LOCALAPPDATA"),
            std::env::var("USERPROFILE"),
        ) {
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
            let mut opera = chromium_cookie_files(&Path::new(&home).join(
                "AppData/Roaming/Opera Software/user_windows",
            ));
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
                ("edge", PathBuf::from(if cfg!(target_os = "macos") { "Microsoft Edge" } else { "microsoft-edge" })),
                ("chrome", PathBuf::from(if cfg!(target_os = "macos") { "Google/Chrome" } else { "google-chrome" })),
                ("brave", PathBuf::from(if cfg!(target_os = "macos") { "BraveSoftware/Brave Browser" } else { "brave-browser" })),
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

/// Run a short yt-dlp invocation and capture its output, bounded by a timeout.
fn run_yt_dlp_capture(args: &[&str], timeout: Duration) -> Result<std::process::Output, String> {
    let mut cmd = silent_command(python_path());
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
pub fn import_youtube(
    input: &str,
    on_progress: impl FnMut(YtImportProgress),
) -> Result<PathBuf, String> {
    match import_youtube_impl(input, on_progress) {
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
        .arg("--progress")
        .arg("--no-warnings");

    for arg in yt_cookies_args() {
        cmd.arg(arg);
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
    }

    result
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
