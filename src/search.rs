use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::download::{DownloadFormat, parse_download_format_str};
use crate::error::{Result, TermusicError};
use crate::paths::{DEFAULT_SEARCH_LIMIT, validate_search_limit};
use crate::ytdlp::{self, ToolEnv};

const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 512 * 1024;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of curated search providers supported by `termusic search -p`.
pub const SUPPORTED_SEARCH_PROVIDER_COUNT: usize = 2;

/// Curated search providers supported via `yt-dlp` keyword search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SearchProvider {
    #[default]
    #[serde(rename = "youtube")]
    YouTube,
    #[serde(rename = "soundcloud")]
    SoundCloud,
}

impl SearchProvider {
    /// Canonical lowercase identifier stored in config and CLI help.
    pub fn id(self) -> &'static str {
        match self {
            Self::YouTube => "youtube",
            Self::SoundCloud => "soundcloud",
        }
    }

    /// Human-friendly provider name for TUI headers and status messages.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::YouTube => "YouTube",
            Self::SoundCloud => "SoundCloud",
        }
    }

    /// `yt-dlp` search prefix used before `{limit}:{query}`.
    pub fn yt_dlp_prefix(self) -> &'static str {
        match self {
            Self::YouTube => "ytsearch",
            Self::SoundCloud => "scsearch",
        }
    }

    /// Parse a provider name, prefix, or website URL from the curated supported list.
    pub fn parse(raw: &str) -> Result<Self> {
        let normalized = normalize_provider_token(raw);
        match normalized.as_str() {
            "youtube" | "yt" | "ytsearch" | "youtube.com" | "www.youtube.com" | "m.youtube.com"
            | "youtu.be" => Ok(Self::YouTube),
            "soundcloud" | "sc" | "scsearch" | "soundcloud.com" | "www.soundcloud.com"
            | "m.soundcloud.com" => Ok(Self::SoundCloud),
            _ => Err(TermusicError::InvalidArg(format!(
                "unsupported search provider '{raw}'; {SUPPORTED_SEARCH_PROVIDER_COUNT} supported search providers: YouTube (youtube, youtube.com, youtu.be) and SoundCloud (soundcloud, soundcloud.com)"
            ))),
        }
    }
}

impl fmt::Display for SearchProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

fn normalize_provider_token(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(&trimmed);
    let host_and_port = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = match host_and_port.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host_and_port,
    };
    host.trim_end_matches('.').to_string()
}

/// Parsed options for `termusic search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub limit: Option<usize>,
    pub set_limit: Option<usize>,
    pub no_tui: bool,
    pub download: bool,
    pub dest: Option<PathBuf>,
    pub provider: Option<SearchProvider>,
    pub provider_default: Option<SearchProvider>,
    pub format: Option<DownloadFormat>,
    pub format_default: Option<DownloadFormat>,
}

/// Normalized metadata for a single `yt-dlp` search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub url: String,
    pub channel: Option<String>,
    pub duration_secs: Option<u64>,
    pub view_count: Option<u64>,
    pub is_live: bool,
}

/// Resolve the effective search limit using precedence:
/// `--limit` -> `--set-limit` -> `TERMUSIC_SEARCH_LIMIT` -> `config.search_limit` -> `DEFAULT_SEARCH_LIMIT`.
pub fn resolve_search_limit(
    opts: &SearchOptions,
    env_limit: Option<&str>,
    config_limit: Option<usize>,
) -> Result<usize> {
    if let Some(limit) = opts.limit {
        return validate_search_limit(limit);
    }
    if let Some(limit) = opts.set_limit {
        return validate_search_limit(limit);
    }
    if let Some(raw) = env_limit.map(str::trim).filter(|s| !s.is_empty()) {
        let parsed = raw.parse::<usize>().map_err(|err| {
            TermusicError::InvalidArg(format!(
                "invalid TERMUSIC_SEARCH_LIMIT '{raw}': {err}"
            ))
        })?;
        return validate_search_limit(parsed);
    }
    if let Some(limit) = config_limit {
        return validate_search_limit(limit);
    }
    Ok(DEFAULT_SEARCH_LIMIT)
}

/// Resolve the effective search provider using precedence:
/// `--provider` -> `--provider-default` -> `TERMUSIC_SEARCH_PROVIDER` -> `config.search_provider` -> `YouTube`.
pub fn resolve_search_provider(
    opts: &SearchOptions,
    env_provider: Option<&str>,
    config_provider: Option<SearchProvider>,
) -> Result<SearchProvider> {
    if let Some(provider) = opts.provider {
        return Ok(provider);
    }
    if let Some(provider) = opts.provider_default {
        return Ok(provider);
    }
    if let Some(raw) = env_provider.map(str::trim).filter(|s| !s.is_empty()) {
        return SearchProvider::parse(raw);
    }
    Ok(config_provider.unwrap_or_default())
}

/// Resolve the effective download format for `search -d` using precedence:
/// `--format` -> `--format-default` -> `TERMUSIC_SEARCH_FORMAT` -> `config.search_download_format` -> `Mp3`.
pub fn resolve_search_format(
    opts: &SearchOptions,
    env_format: Option<&str>,
    config_format: Option<DownloadFormat>,
) -> Result<DownloadFormat> {
    if let Some(format) = opts.format {
        return Ok(format);
    }
    if let Some(format) = opts.format_default {
        return Ok(format);
    }
    if let Some(raw) = env_format.map(str::trim).filter(|s| !s.is_empty()) {
        return parse_download_format_str(raw);
    }
    Ok(config_format.unwrap_or_default())
}

/// Build the `yt-dlp` argument vector for a flat-playlist metadata search on `provider`.
pub fn build_yt_dlp_search_args(
    query: &str,
    limit: usize,
    provider: SearchProvider,
    js_runtime: Option<&str>,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--flat-playlist"),
        OsString::from("--dump-single-json"),
        OsString::from("--simulate"),
        OsString::from("--no-warnings"),
    ];
    if let Some(runtime) = js_runtime.map(str::trim).filter(|s| !s.is_empty()) {
        args.push(OsString::from("--js-runtimes"));
        args.push(OsString::from(runtime));
    }
    args.push(OsString::from("--"));
    args.push(OsString::from(format!(
        "{}{limit}:{query}",
        provider.yt_dlp_prefix()
    )));
    args
}

#[derive(Debug, Deserialize)]
struct RawSearchResponse {
    #[serde(default)]
    entries: Option<Vec<Option<RawSearchEntry>>>,
}

#[derive(Debug, Deserialize)]
struct RawSearchEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    view_count: Option<u64>,
    #[serde(default)]
    live_status: Option<String>,
    #[serde(default)]
    ie_key: Option<String>,
}

fn non_empty_trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn resolve_entry_url(
    id: &str,
    url: Option<String>,
    webpage_url: Option<String>,
    ie_key: Option<&str>,
    provider: SearchProvider,
) -> Option<String> {
    // Prefer `webpage_url` first so SoundCloud returns `https://soundcloud.com/...` instead of `api.soundcloud.com/...`.
    if let Some(wu) = non_empty_trimmed(webpage_url)
        && is_http_url(&wu)
    {
        return Some(wu);
    }
    if let Some(u) = non_empty_trimmed(url)
        && is_http_url(&u)
    {
        return Some(u);
    }
    if provider == SearchProvider::YouTube
        && !id.is_empty()
        && ie_key.is_none_or(|k| k.eq_ignore_ascii_case("Youtube"))
    {
        return Some(format!("https://www.youtube.com/watch?v={id}"));
    }
    None
}

/// Parse `yt-dlp --flat-playlist --dump-single-json` output for YouTube into [`SearchResult`] items.
pub fn parse_yt_dlp_search_json(json_bytes: &[u8]) -> Result<Vec<SearchResult>> {
    parse_yt_dlp_search_json_for_provider(json_bytes, SearchProvider::YouTube)
}

/// Parse `yt-dlp --flat-playlist --dump-single-json` output for `provider` into [`SearchResult`] items.
pub fn parse_yt_dlp_search_json_for_provider(
    json_bytes: &[u8],
    provider: SearchProvider,
) -> Result<Vec<SearchResult>> {
    let raw: RawSearchResponse = serde_json::from_slice(json_bytes)
        .map_err(|err| TermusicError::Search(format!("failed to parse yt-dlp search JSON: {err}")))?;

    let mut results = Vec::new();
    for entry in raw.entries.into_iter().flatten().flatten() {
        let id = non_empty_trimmed(entry.id).unwrap_or_default();
        let ie_key = non_empty_trimmed(entry.ie_key);
        let Some(url) = resolve_entry_url(
            &id,
            entry.url,
            entry.webpage_url,
            ie_key.as_deref(),
            provider,
        ) else {
            continue;
        };
        let title = non_empty_trimmed(entry.title).unwrap_or_else(|| "(untitled)".to_string());
        let channel = non_empty_trimmed(entry.channel).or_else(|| non_empty_trimmed(entry.uploader));
        let duration_secs = entry
            .duration
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d.round() as u64);
        let is_live = entry.live_status.as_deref() == Some("is_live");

        results.push(SearchResult {
            index: results.len() + 1,
            id,
            title,
            url,
            channel,
            duration_secs,
            view_count: entry.view_count,
            is_live,
        });
    }

    Ok(results)
}

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    max_bytes: usize,
    fail_on_overflow: bool,
) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(buf.len());
        if n > remaining {
            if fail_on_overflow {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("output exceeded {max_bytes} byte limit"),
                ));
            }
            if remaining > 0 {
                buf.extend_from_slice(&chunk[..remaining]);
            }
            // Drain remaining bytes without storing so the child does not hit EPIPE on stderr.
            continue;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    Ok(buf)
}

/// Run a `yt-dlp` metadata search for `query` on `provider` returning up to `limit` results.
pub async fn search_yt_dlp(
    query: &str,
    limit: usize,
    provider: SearchProvider,
) -> Result<Vec<SearchResult>> {
    let env = ToolEnv::from_system();
    search_yt_dlp_with_env_and_timeout(query, limit, provider, &env, SEARCH_TIMEOUT).await
}

async fn search_yt_dlp_with_env_and_timeout(
    query: &str,
    limit: usize,
    provider: SearchProvider,
    env: &ToolEnv,
    timeout: Duration,
) -> Result<Vec<SearchResult>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(TermusicError::InvalidArg(
            "search query cannot be empty".to_string(),
        ));
    }
    let valid_limit = validate_search_limit(limit)?;

    let yt_dlp_bin = ytdlp::find_yt_dlp(env)?;
    let js_runtime = ytdlp::detect_js_runtime(env);
    let args = build_yt_dlp_search_args(trimmed, valid_limit, provider, js_runtime.as_deref());

    let mut cmd = ytdlp::command_for_binary(&yt_dlp_bin, &args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|err| TermusicError::Search(format!("failed to start yt-dlp: {err}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| TermusicError::Search("yt-dlp stdout pipe missing".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| TermusicError::Search("yt-dlp stderr pipe missing".to_string()))?;

    let io_and_wait = async {
        tokio::try_join!(
            read_bounded(stdout, MAX_STDOUT_BYTES, true),
            read_bounded(stderr, MAX_STDERR_BYTES, false),
            child.wait(),
        )
    };

    let (stdout_bytes, stderr_bytes, status) = match tokio::time::timeout(timeout, io_and_wait).await
    {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(err)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(TermusicError::Search(format!(
                "yt-dlp search failed: {err}"
            )));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(TermusicError::Search(format!(
                "yt-dlp search timed out after {}s",
                timeout.as_secs()
            )));
        }
    };

    if !status.success() {
        let code_info = status
            .code()
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "terminated by signal".to_string());
        let stderr_text = String::from_utf8_lossy(&stderr_bytes);
        let detail = match ytdlp::extract_diagnostic_line(&stderr_text) {
            Some(line) => format!("yt-dlp search failed ({code_info}): {line}"),
            None => format!("yt-dlp search failed ({code_info})"),
        };
        return Err(TermusicError::Search(detail));
    }

    parse_yt_dlp_search_json_for_provider(&stdout_bytes, provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    struct TestBinDir {
        root: PathBuf,
        bin_dir: PathBuf,
    }

    impl TestBinDir {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "termusic-search-test-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            let bin_dir = root.join("bin");
            fs::create_dir_all(&bin_dir).unwrap();
            Self { root, bin_dir }
        }

        fn env(&self) -> ToolEnv {
            ToolEnv {
                home_dir: Some(self.root.join("home")),
                path_var: Some(OsString::from(self.bin_dir.as_os_str())),
                pathext: None,
                extra_search_dirs: Vec::new(),
            }
        }
    }

    impl Drop for TestBinDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_executable(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[cfg(windows)]
    fn js_escape(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    fn write_mock_yt_dlp(bin_dir: &Path, unix_body: &str, win_js_body: &str) {
        #[cfg(unix)]
        {
            let _ = win_js_body;
            let script = format!("#!/bin/sh\nset -eu\n{unix_body}\n");
            write_executable(&bin_dir.join("yt-dlp"), &script);
        }
        #[cfg(windows)]
        {
            let _ = unix_body;
            let js_path = bin_dir.join("yt-dlp-mock.js");
            let js_script = format!(
                "var fso = new ActiveXObject(\"Scripting.FileSystemObject\");\n\
                 var allArgs = [];\n\
                 for (var i = 0; i < WScript.Arguments.Length; i++) {{\n\
                   allArgs.push(WScript.Arguments(i));\n\
                 }}\n\
                 {win_js_body}\n"
            );
            fs::write(&js_path, js_script).unwrap();
            write_executable(
                &bin_dir.join("yt-dlp.cmd"),
                "@\"%SystemRoot%\\System32\\cscript.exe\" //Nologo //E:JScript \"%~dp0yt-dlp-mock.js\" %*\r\n",
            );
        }
    }

    fn base_opts() -> SearchOptions {
        SearchOptions {
            query: "lofi".to_string(),
            limit: None,
            set_limit: None,
            no_tui: false,
            download: false,
            dest: None,
            provider: None,
            provider_default: None,
            format: None,
            format_default: None,
        }
    }

    #[test]
    fn search_provider_parse_normalizes_urls_and_rejects_unsupported() {
        assert_eq!(SUPPORTED_SEARCH_PROVIDER_COUNT, 2);
        for raw in [
            "youtube",
            "YT",
            "ytsearch",
            "youtube.com",
            "https://www.youtube.com/results?search_query=lofi",
            "http://m.youtube.com:80/",
            "https://youtu.be/dQw4w9WgXcQ",
        ] {
            assert_eq!(
                SearchProvider::parse(raw).unwrap(),
                SearchProvider::YouTube,
                "raw={raw}"
            );
        }

        for raw in [
            "soundcloud",
            "sc",
            "SCSEARCH",
            "soundcloud.com",
            "https://www.soundcloud.com/discover",
            "http://m.soundcloud.com/",
        ] {
            assert_eq!(
                SearchProvider::parse(raw).unwrap(),
                SearchProvider::SoundCloud,
                "raw={raw}"
            );
        }

        for bad in ["", "   ", "https://vimeo.com", "spotify", "music.youtube.com"] {
            let err = SearchProvider::parse(bad).unwrap_err();
            match err {
                TermusicError::InvalidArg(msg) => {
                    assert!(msg.contains("2 supported search providers"), "msg={msg}");
                    assert!(msg.contains("YouTube"), "msg={msg}");
                    assert!(msg.contains("SoundCloud"), "msg={msg}");
                }
                other => panic!("expected InvalidArg, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolve_search_provider_and_format_respect_precedence() {
        let mut opts = base_opts();
        assert_eq!(
            resolve_search_provider(&opts, None, None).unwrap(),
            SearchProvider::YouTube
        );
        assert_eq!(
            resolve_search_provider(&opts, Some("  "), Some(SearchProvider::SoundCloud)).unwrap(),
            SearchProvider::SoundCloud
        );
        assert_eq!(
            resolve_search_provider(&opts, Some("soundcloud.com"), Some(SearchProvider::YouTube))
                .unwrap(),
            SearchProvider::SoundCloud
        );
        opts.provider_default = Some(SearchProvider::YouTube);
        assert_eq!(
            resolve_search_provider(&opts, Some("soundcloud"), Some(SearchProvider::SoundCloud))
                .unwrap(),
            SearchProvider::YouTube
        );
        opts.provider = Some(SearchProvider::SoundCloud);
        assert_eq!(
            resolve_search_provider(&opts, Some("youtube"), Some(SearchProvider::YouTube)).unwrap(),
            SearchProvider::SoundCloud
        );

        let bare = base_opts();
        assert!(matches!(
            resolve_search_provider(&bare, Some("vimeo.com"), None),
            Err(TermusicError::InvalidArg(_))
        ));

        assert_eq!(
            resolve_search_format(&bare, None, None).unwrap(),
            DownloadFormat::Mp3
        );
        assert_eq!(
            resolve_search_format(&bare, Some(" "), Some(DownloadFormat::Mp4)).unwrap(),
            DownloadFormat::Mp4
        );
        assert_eq!(
            resolve_search_format(&bare, Some(".mp4"), Some(DownloadFormat::Flac)).unwrap(),
            DownloadFormat::Mp4
        );
        let mut fmt_opts = base_opts();
        fmt_opts.format_default = Some(DownloadFormat::Flac);
        assert_eq!(
            resolve_search_format(&fmt_opts, Some("mp4"), Some(DownloadFormat::Mp3)).unwrap(),
            DownloadFormat::Flac
        );
        fmt_opts.format = Some(DownloadFormat::Mp4);
        assert_eq!(
            resolve_search_format(&fmt_opts, Some("mp3"), Some(DownloadFormat::Flac)).unwrap(),
            DownloadFormat::Mp4
        );
        assert!(matches!(
            resolve_search_format(&bare, Some(".mkv"), None),
            Err(TermusicError::InvalidArg(_))
        ));
    }

    #[test]
    fn resolve_search_limit_respects_precedence_and_validates_env() {
        let mut opts = base_opts();
        assert_eq!(resolve_search_limit(&opts, None, None).unwrap(), DEFAULT_SEARCH_LIMIT);
        assert_eq!(resolve_search_limit(&opts, Some("   "), Some(15)).unwrap(), 15);
        assert_eq!(resolve_search_limit(&opts, Some(" 25 "), Some(15)).unwrap(), 25);

        opts.set_limit = Some(30);
        assert_eq!(resolve_search_limit(&opts, Some("25"), Some(15)).unwrap(), 30);

        opts.limit = Some(5);
        assert_eq!(resolve_search_limit(&opts, Some("25"), Some(15)).unwrap(), 5);

        // Invalid env values fail cleanly when not overridden by CLI flags.
        let bare = base_opts();
        for bad in ["0", "101", "-2", "abc"] {
            let err = resolve_search_limit(&bare, Some(bad), None).unwrap_err();
            assert!(matches!(err, TermusicError::InvalidArg(_)), "bad={bad}");
        }

        let mut bad_flag = base_opts();
        bad_flag.limit = Some(0);
        assert!(matches!(
            resolve_search_limit(&bad_flag, None, None),
            Err(TermusicError::InvalidArg(_))
        ));
    }

    #[test]
    fn build_yt_dlp_search_args_includes_flags_runtime_and_separator() {
        let without_rt =
            build_yt_dlp_search_args("-leading dash", 5, SearchProvider::YouTube, None);
        assert_eq!(
            without_rt,
            vec![
                OsString::from("--flat-playlist"),
                OsString::from("--dump-single-json"),
                OsString::from("--simulate"),
                OsString::from("--no-warnings"),
                OsString::from("--"),
                OsString::from("ytsearch5:-leading dash"),
            ]
        );

        let with_rt = build_yt_dlp_search_args(
            "lofi",
            10,
            SearchProvider::SoundCloud,
            Some("deno:/usr/bin/deno"),
        );
        assert_eq!(
            with_rt,
            vec![
                OsString::from("--flat-playlist"),
                OsString::from("--dump-single-json"),
                OsString::from("--simulate"),
                OsString::from("--no-warnings"),
                OsString::from("--js-runtimes"),
                OsString::from("deno:/usr/bin/deno"),
                OsString::from("--"),
                OsString::from("scsearch10:lofi"),
            ]
        );
    }

    #[test]
    fn parse_yt_dlp_search_json_handles_flat_playlist_quirks() {
        let empty = br#"{"_type": "playlist", "entries": []}"#;
        assert!(parse_yt_dlp_search_json(empty).unwrap().is_empty());

        let null_entries = br#"{"_type": "playlist", "entries": null}"#;
        assert!(parse_yt_dlp_search_json(null_entries).unwrap().is_empty());

        let payload = br#"{
            "_type": "playlist",
            "entries": [
                null,
                {
                    "id": "dQw4w9WgXcQ",
                    "title": "Never Gonna Give You Up",
                    "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                    "duration": 213.6,
                    "channel": "Rick Astley",
                    "uploader": "Rick Astley",
                    "view_count": 1836332720,
                    "live_status": null
                },
                {
                    "id": "rFZHOHl-L8A",
                    "title": "lofi hip hop radio",
                    "url": "rFZHOHl-L8A",
                    "duration": null,
                    "channel": "  ",
                    "uploader": "Lofi Girl",
                    "view_count": null,
                    "live_status": "is_live"
                }
            ]
        }"#;

        let parsed = parse_yt_dlp_search_json(payload).unwrap();
        assert_eq!(parsed.len(), 2);

        assert_eq!(
            parsed[0],
            SearchResult {
                index: 1,
                id: "dQw4w9WgXcQ".to_string(),
                title: "Never Gonna Give You Up".to_string(),
                url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
                channel: Some("Rick Astley".to_string()),
                duration_secs: Some(214),
                view_count: Some(1_836_332_720),
                is_live: false,
            }
        );

        assert_eq!(
            parsed[1],
            SearchResult {
                index: 2,
                id: "rFZHOHl-L8A".to_string(),
                title: "lofi hip hop radio".to_string(),
                url: "https://www.youtube.com/watch?v=rFZHOHl-L8A".to_string(),
                channel: Some("Lofi Girl".to_string()),
                duration_secs: None,
                view_count: None,
                is_live: true,
            }
        );

        assert!(matches!(
            parse_yt_dlp_search_json(b"{not-json"),
            Err(TermusicError::Search(_))
        ));

        // SoundCloud prefers `webpage_url` over `api.soundcloud.com` `url`, and does not synthesize YouTube URLs.
        let sc_payload = br#"{
            "_type": "playlist",
            "entries": [
                {
                    "id": "417474360",
                    "title": "Chill Study Beats",
                    "url": "https://api.soundcloud.com/tracks/soundcloud%3Atracks%3A417474360",
                    "webpage_url": "https://soundcloud.com/nigeldelviero/chill-study-beats",
                    "uploader": "nigeldelviero",
                    "channel": null,
                    "duration": 7200.255,
                    "view_count": 4867636,
                    "ie_key": "Soundcloud"
                },
                {
                    "id": "999999",
                    "title": "Missing URL Track",
                    "url": "999999",
                    "webpage_url": null,
                    "ie_key": "Soundcloud"
                }
            ]
        }"#;
        let sc_parsed =
            parse_yt_dlp_search_json_for_provider(sc_payload, SearchProvider::SoundCloud).unwrap();
        assert_eq!(sc_parsed.len(), 1);
        assert_eq!(
            sc_parsed[0],
            SearchResult {
                index: 1,
                id: "417474360".to_string(),
                title: "Chill Study Beats".to_string(),
                url: "https://soundcloud.com/nigeldelviero/chill-study-beats".to_string(),
                channel: Some("nigeldelviero".to_string()),
                duration_secs: Some(7200),
                view_count: Some(4_867_636),
                is_live: false,
            }
        );
    }

    #[tokio::test]
    async fn search_yt_dlp_validates_inputs_before_spawning() {
        let dir = TestBinDir::new("validate");
        let env = dir.env();

        assert!(matches!(
            search_yt_dlp_with_env_and_timeout(
                "   ",
                10,
                SearchProvider::YouTube,
                &env,
                SEARCH_TIMEOUT
            )
            .await,
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            search_yt_dlp_with_env_and_timeout(
                "lofi",
                0,
                SearchProvider::YouTube,
                &env,
                SEARCH_TIMEOUT
            )
            .await,
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            search_yt_dlp_with_env_and_timeout(
                "lofi",
                101,
                SearchProvider::YouTube,
                &env,
                SEARCH_TIMEOUT
            )
            .await,
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            search_yt_dlp_with_env_and_timeout(
                "lofi",
                5,
                SearchProvider::YouTube,
                &env,
                SEARCH_TIMEOUT
            )
            .await,
            Err(TermusicError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn search_yt_dlp_executes_mock_and_parses_results() {
        let dir = TestBinDir::new("ok");
        let args_log = dir.root.join("args.txt");
        let json_payload = r#"{"_type":"playlist","entries":[{"id":"abc123","title":"Mock Track","url":"https://www.youtube.com/watch?v=abc123","duration":125.0,"channel":"Mock Artist","view_count":42}]}"#;
        let json_file = dir.root.join("resp.json");
        fs::write(&json_file, json_payload).unwrap();

        write_mock_yt_dlp(
            &dir.bin_dir,
            &format!(
                "printf '%s\\n' \"$@\" > \"{}\"\ncat \"{}\"",
                args_log.display(),
                json_file.display()
            ),
            &format!(
                "var log = fso.CreateTextFile(\"{}\", true);\n\
                 for (var j = 0; j < allArgs.length; j++) log.WriteLine(allArgs[j]);\n\
                 log.Close();\n\
                 var f = fso.OpenTextFile(\"{}\", 1);\n\
                 WScript.StdOut.Write(f.ReadAll());\n\
                 f.Close();",
                {
                    #[cfg(windows)]
                    {
                        js_escape(&args_log)
                    }
                    #[cfg(not(windows))]
                    {
                        ""
                    }
                },
                {
                    #[cfg(windows)]
                    {
                        js_escape(&json_file)
                    }
                    #[cfg(not(windows))]
                    {
                        ""
                    }
                }
            ),
        );

        let results = search_yt_dlp_with_env_and_timeout(
            "lofi beats",
            5,
            SearchProvider::SoundCloud,
            &dir.env(),
            SEARCH_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "abc123");
        assert_eq!(results[0].title, "Mock Track");
        assert_eq!(results[0].duration_secs, Some(125));
        assert_eq!(results[0].channel.as_deref(), Some("Mock Artist"));

        let recorded = fs::read_to_string(&args_log).unwrap();
        let recorded_lines: Vec<&str> = recorded.lines().collect();
        assert!(recorded_lines.contains(&"--flat-playlist"));
        assert!(recorded_lines.contains(&"--dump-single-json"));
        assert!(recorded_lines.contains(&"--simulate"));
        assert!(recorded_lines.contains(&"--no-warnings"));
        assert!(recorded_lines.contains(&"--"));
        assert!(recorded_lines.contains(&"scsearch5:lofi beats"));
    }

    #[tokio::test]
    async fn search_yt_dlp_surfaces_stderr_diagnostic_on_failure() {
        let dir = TestBinDir::new("err");
        write_mock_yt_dlp(
            &dir.bin_dir,
            "echo 'WARNING: transient' >&2\necho 'ERROR: HTTP Error 429' >&2\nexit 1",
            "WScript.StdErr.WriteLine('WARNING: transient');\n\
             WScript.StdErr.WriteLine('ERROR: HTTP Error 429');\n\
             WScript.Quit(1);",
        );

        let err = search_yt_dlp_with_env_and_timeout(
            "lofi",
            3,
            SearchProvider::YouTube,
            &dir.env(),
            SEARCH_TIMEOUT,
        )
        .await
        .unwrap_err();
        match err {
            TermusicError::Search(msg) => {
                assert!(msg.contains("ERROR: HTTP Error 429"), "msg={msg}");
            }
            other => panic!("expected Search error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_yt_dlp_errors_when_stdout_exceeds_cap() {
        let dir = TestBinDir::new("overflow");
        let big_file = dir.root.join("big.json");
        let oversized = vec![b' '; MAX_STDOUT_BYTES + 4096];
        fs::write(&big_file, oversized).unwrap();

        write_mock_yt_dlp(
            &dir.bin_dir,
            &format!("cat \"{}\"", big_file.display()),
            &format!(
                "var f = fso.OpenTextFile(\"{}\", 1);\n\
                 WScript.StdOut.Write(f.ReadAll());\n\
                 f.Close();",
                {
                    #[cfg(windows)]
                    {
                        js_escape(&big_file)
                    }
                    #[cfg(not(windows))]
                    {
                        ""
                    }
                }
            ),
        );

        let err = search_yt_dlp_with_env_and_timeout(
            "lofi",
            3,
            SearchProvider::YouTube,
            &dir.env(),
            SEARCH_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TermusicError::Search(_)));
    }

    #[tokio::test]
    async fn search_yt_dlp_times_out_when_child_hangs() {
        let dir = TestBinDir::new("timeout");
        write_mock_yt_dlp(
            &dir.bin_dir,
            "sleep 5",
            "WScript.Sleep(5000);",
        );

        let err = search_yt_dlp_with_env_and_timeout(
            "lofi",
            3,
            SearchProvider::YouTube,
            &dir.env(),
            Duration::from_millis(80),
        )
        .await
        .unwrap_err();
        match err {
            TermusicError::Search(msg) => assert!(msg.contains("timed out"), "msg={msg}"),
            other => panic!("expected timeout Search error, got {other:?}"),
        }
    }
}
