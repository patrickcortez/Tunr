use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::error::{Result, TermusicError};
use crate::library::{AddSummary, Library};
use crate::paths::{AppPaths, load_workspace_config, set_workspace_download_dest};
use crate::platform::canonicalize_clean;
use crate::ytdlp::{self, ToolEnv};

/// Output container/codec format supported by `termusic download` and `termusic search -d`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DownloadFormat {
    #[default]
    Mp3,
    Mp4,
    Flac,
    M4a,
    Ogg,
    Wav,
    Aac,
}

impl DownloadFormat {
    /// Canonical lowercase format name used by termusic.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Mp4 => "mp4",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Ogg => "ogg",
            Self::Wav => "wav",
            Self::Aac => "aac",
        }
    }

    /// Return `true` for audio-only formats eligible for music library indexing.
    pub fn is_audio(self) -> bool {
        !matches!(self, Self::Mp4)
    }

    /// Format argument expected by `yt-dlp --audio-format` (`ogg` -> `vorbis`).
    pub fn yt_dlp_format(self) -> &'static str {
        match self {
            Self::Ogg => "vorbis",
            other => other.as_str(),
        }
    }
}

/// Parse a download format string, allowing an optional leading `.` (e.g. `.mp3`, `mp4`).
pub fn parse_download_format_str(raw: &str) -> Result<DownloadFormat> {
    let trimmed = raw.trim();
    let normalized = trimmed.strip_prefix('.').unwrap_or(trimmed).to_ascii_lowercase();
    match normalized.as_str() {
        "mp3" => Ok(DownloadFormat::Mp3),
        "mp4" => Ok(DownloadFormat::Mp4),
        "flac" => Ok(DownloadFormat::Flac),
        "m4a" => Ok(DownloadFormat::M4a),
        "ogg" => Ok(DownloadFormat::Ogg),
        "wav" => Ok(DownloadFormat::Wav),
        "aac" => Ok(DownloadFormat::Aac),
        _ => Err(TermusicError::InvalidArg(format!(
            "unsupported format '{raw}' (expected one of: mp3, mp4, flac, m4a, ogg, wav, aac)"
        ))),
    }
}

impl fmt::Display for DownloadFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parameters controlling `termusic download`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOptions {
    pub urls: Vec<String>,
    pub dest: Option<PathBuf>,
    pub set_dest: Option<PathBuf>,
    pub format: DownloadFormat,
    pub playlist: bool,
    pub embed_thumbnail: bool,
    pub auto_add: bool,
    pub playlist_name: Option<String>,
    pub jobs: usize,
    pub no_tui: bool,
}

/// Lifecycle phase for an individual URL inside `termusic download`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadPhase {
    Queued,
    Starting,
    Downloading {
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
        speed_bytes_per_sec: Option<u64>,
        eta_secs: Option<u64>,
    },
    PostProcessing {
        stage: String,
    },
    Finalizing,
    Completed {
        files: Vec<PathBuf>,
    },
    Failed {
        error: String,
    },
}

/// Real-time batch and per-track progress events emitted by [`run_download_with_progress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadProgressEvent {
    BatchStarted {
        urls: Vec<String>,
        dest_dir: PathBuf,
        format: DownloadFormat,
        jobs: usize,
    },
    Notice(String),
    ItemUpdate {
        index: usize,
        title: Option<String>,
        phase: DownloadPhase,
    },
    IndexingStarted {
        track_count: usize,
    },
    BatchFinished,
}

const PROGRESS_TEMPLATE_DOWNLOAD: &str = "download:TERMUSIC_PROG\t%(progress.status)s\t%(progress.downloaded_bytes)s\t%(progress.total_bytes)s\t%(progress.total_bytes_estimate)s\t%(progress.speed)s\t%(progress.eta)s\t%(info.title)s";
const PROGRESS_TEMPLATE_POSTPROCESS: &str = "postprocess:TERMUSIC_POST\t%(progress.status)s\t%(progress.postprocessor)s\t%(info.title)s";
const MAX_CAPTURE_BYTES: usize = 512 * 1024;

/// Failure details captured for an individual URL during `termusic download`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadFailure {
    pub url: String,
    pub error: String,
}

/// Result returned by [`run_download`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DownloadOutcome {
    pub saved_dest: Option<PathBuf>,
    pub downloaded_files: Vec<PathBuf>,
    pub failed: Vec<DownloadFailure>,
    pub add_summary: Option<AddSummary>,
}

/// Environment snapshot injected into [`run_download_with_env`] so unit tests
/// remain isolated without mutating process-wide environment variables.
#[derive(Debug, Clone)]
struct DownloadEnv {
    home_dir: Option<PathBuf>,
    path_var: Option<OsString>,
    termusic_dest: Option<String>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pathext: Option<OsString>,
    extra_search_dirs: Vec<PathBuf>,
}

impl DownloadEnv {
    fn from_system() -> Self {
        let tool = ToolEnv::from_system();
        Self {
            home_dir: tool.home_dir,
            path_var: tool.path_var,
            termusic_dest: std::env::var("TERMUSIC_DEST").ok(),
            pathext: tool.pathext,
            extra_search_dirs: tool.extra_search_dirs,
        }
    }

    fn to_tool_env(&self) -> ToolEnv {
        ToolEnv {
            home_dir: self.home_dir.clone(),
            path_var: self.path_var.clone(),
            pathext: self.pathext.clone(),
            extra_search_dirs: self.extra_search_dirs.clone(),
        }
    }
}

/// Shared immutable tool/destination context plus destination finalizer lock for one download run.
struct DownloadContext {
    yt_dlp_bin: PathBuf,
    ffmpeg_location: Option<PathBuf>,
    dest_dir: PathBuf,
    staging_root: PathBuf,
    format: DownloadFormat,
    playlist: bool,
    js_runtime: Option<String>,
    embed_thumbnail: bool,
    env: DownloadEnv,
    finalizer_lock: Mutex<()>,
    progress_tx: Option<mpsc::UnboundedSender<DownloadProgressEvent>>,
}

impl DownloadContext {
    fn emit(&self, event: DownloadProgressEvent) {
        emit_progress(self.progress_tx.as_ref(), event);
    }
}

fn emit_progress(
    tx: Option<&mpsc::UnboundedSender<DownloadProgressEvent>>,
    event: DownloadProgressEvent,
) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

/// RAII staging directory guard that cleans up `<dest>/.termusic-dl-tmp/<uuid>` on drop or explicit cleanup.
struct StageDirGuard {
    path: PathBuf,
    cleaned: bool,
}

impl StageDirGuard {
    async fn create(root: &Path) -> Result<Self> {
        let path = root.join(uuid::Uuid::new_v4().to_string());
        tokio::fs::create_dir_all(&path).await?;
        Ok(Self {
            path,
            cleaned: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn cleanup(mut self) {
        self.cleaned = true;
        let _ = tokio::fs::remove_dir_all(&self.path).await;
    }
}

impl Drop for StageDirGuard {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Validate non-empty URL arguments, split whitespace-delimited tokens within each argument,
/// and deduplicate while preserving first-seen order.
pub fn normalize_urls(raw_urls: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();

    for raw in raw_urls {
        if raw.trim().is_empty() {
            return Err(TermusicError::InvalidArg(
                "URL cannot be empty".to_string(),
            ));
        }
        for token in raw.split_whitespace() {
            if seen.insert(token.to_string()) {
                normalized.push(token.to_string());
            }
        }
    }

    Ok(normalized)
}

async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| TermusicError::Async(format!("blocking task failed: {err}")))?
}

/// Download tracks concurrently with `yt-dlp` and optionally index them into the workspace library.
pub async fn run_download(paths: &AppPaths, opts: &DownloadOptions) -> Result<DownloadOutcome> {
    run_download_with_progress(paths, opts, None).await
}

/// Download tracks concurrently with `yt-dlp` while streaming batch/per-item progress events.
pub async fn run_download_with_progress(
    paths: &AppPaths,
    opts: &DownloadOptions,
    progress_tx: Option<mpsc::UnboundedSender<DownloadProgressEvent>>,
) -> Result<DownloadOutcome> {
    let env = DownloadEnv::from_system();
    run_download_with_env_and_progress(paths, opts, &env, progress_tx).await
}

#[cfg(test)]
async fn run_download_with_env(
    paths: &AppPaths,
    opts: &DownloadOptions,
    env: &DownloadEnv,
) -> Result<DownloadOutcome> {
    run_download_with_env_and_progress(paths, opts, env, None).await
}

async fn run_download_with_env_and_progress(
    paths: &AppPaths,
    opts: &DownloadOptions,
    env: &DownloadEnv,
    progress_tx: Option<mpsc::UnboundedSender<DownloadProgressEvent>>,
) -> Result<DownloadOutcome> {
    let saved_dest = match opts.set_dest.as_deref() {
        Some(dir) => {
            validate_non_empty_path(dir, "--set-dest")?;
            let paths_cloned = paths.clone();
            let dir_owned = dir.to_path_buf();
            let saved = run_blocking(move || set_workspace_download_dest(&paths_cloned, &dir_owned))
                .await?;
            Some(saved)
        }
        None => None,
    };

    let urls = normalize_urls(&opts.urls)?;
    if urls.is_empty() {
        if saved_dest.is_some() {
            return Ok(DownloadOutcome {
                saved_dest,
                ..DownloadOutcome::default()
            });
        }
        return Err(TermusicError::InvalidArg(
            "at least one download URL is required".into(),
        ));
    }

    let paths_cloned = paths.clone();
    let opts_cloned = opts.clone();
    let saved_cloned = saved_dest.clone();
    let env_cloned = env.clone();
    let (dest_dir, yt_dlp_bin, ffmpeg_bin, js_runtime) = run_blocking(move || {
        let dest_dir = resolve_destination(
            &paths_cloned,
            &opts_cloned,
            saved_cloned.as_deref(),
            &env_cloned,
        )?;
        let yt_dlp_bin = find_yt_dlp(&env_cloned)?;
        let ffmpeg_bin = find_ffmpeg(&env_cloned)?;
        let js_runtime = detect_js_runtime(&env_cloned);
        Ok((dest_dir, yt_dlp_bin, ffmpeg_bin, js_runtime))
    })
    .await?;

    let ffmpeg_location = ffmpeg_bin.parent().map(Path::to_path_buf);
    let embed_thumbnail = should_embed_thumbnail_with_progress(opts, env, progress_tx.as_ref()).await;
    let staging_root = dest_dir.join(".termusic-dl-tmp");
    let effective_jobs = opts.jobs.max(1);
    emit_progress(
        progress_tx.as_ref(),
        DownloadProgressEvent::BatchStarted {
            urls: urls.clone(),
            dest_dir: dest_dir.clone(),
            format: opts.format,
            jobs: effective_jobs,
        },
    );

    let ctx = Arc::new(DownloadContext {
        yt_dlp_bin,
        ffmpeg_location,
        dest_dir,
        staging_root,
        format: opts.format,
        playlist: opts.playlist,
        js_runtime,
        embed_thumbnail,
        env: env.clone(),
        finalizer_lock: Mutex::new(()),
        progress_tx: progress_tx.clone(),
    });

    let semaphore = Arc::new(Semaphore::new(effective_jobs));
    let mut join_set = JoinSet::new();

    for (idx, url) in urls.iter().cloned().enumerate() {
        let ctx = Arc::clone(&ctx);
        let semaphore = Arc::clone(&semaphore);
        join_set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.map_err(|_| {
                TermusicError::Async("download concurrency semaphore closed".to_string())
            })?;
            ctx.emit(DownloadProgressEvent::ItemUpdate {
                index: idx,
                title: None,
                phase: DownloadPhase::Starting,
            });
            let outcome = download_single_url(&ctx, idx, &url).await;
            match &outcome {
                Ok(files) => ctx.emit(DownloadProgressEvent::ItemUpdate {
                    index: idx,
                    title: files
                        .first()
                        .and_then(|p| p.file_stem())
                        .and_then(|s| s.to_str())
                        .map(str::to_string),
                    phase: DownloadPhase::Completed {
                        files: files.clone(),
                    },
                }),
                Err(err) => ctx.emit(DownloadProgressEvent::ItemUpdate {
                    index: idx,
                    title: None,
                    phase: DownloadPhase::Failed {
                        error: err.to_string(),
                    },
                }),
            }
            Ok::<_, TermusicError>((idx, outcome))
        });
    }

    let mut slots: Vec<Option<std::result::Result<Vec<PathBuf>, String>>> = vec![None; urls.len()];
    while let Some(joined) = join_set.join_next().await {
        let (idx, outcome) = joined.map_err(|err| {
            TermusicError::Async(format!("download worker task failed: {err}"))
        })??;
        slots[idx] = Some(outcome.map_err(|err| err.to_string()));
    }

    let _ = tokio::fs::remove_dir(&ctx.staging_root).await;

    let mut downloaded_files = Vec::new();
    let mut failed = Vec::new();
    for (idx, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(Ok(files)) => downloaded_files.extend(files),
            Some(Err(error)) => failed.push(DownloadFailure {
                url: urls[idx].clone(),
                error,
            }),
            None => failed.push(DownloadFailure {
                url: urls[idx].clone(),
                error: "download worker exited without a result".to_string(),
            }),
        }
    }

    let add_summary = if opts.auto_add && !downloaded_files.is_empty() {
        if opts.format.is_audio() {
            emit_progress(
                progress_tx.as_ref(),
                DownloadProgressEvent::IndexingStarted {
                    track_count: downloaded_files.len(),
                },
            );
            let paths_cloned = paths.clone();
            let files_cloned = downloaded_files.clone();
            let playlist_cloned = opts.playlist_name.clone();
            Some(
                run_blocking(move || {
                    index_downloaded_files(
                        &paths_cloned,
                        &files_cloned,
                        playlist_cloned.as_deref(),
                    )
                })
                .await?,
            )
        } else {
            emit_progress(
                progress_tx.as_ref(),
                DownloadProgressEvent::Notice(
                    "skipping library indexing for non-audio mp4 download".to_string(),
                ),
            );
            None
        }
    } else {
        None
    };

    emit_progress(progress_tx.as_ref(), DownloadProgressEvent::BatchFinished);

    Ok(DownloadOutcome {
        saved_dest,
        downloaded_files,
        failed,
        add_summary,
    })
}

async fn download_single_url(
    ctx: &DownloadContext,
    index: usize,
    url: &str,
) -> Result<Vec<PathBuf>> {
    let guard = StageDirGuard::create(&ctx.staging_root).await?;
    let res = download_into_stage_and_finalize(ctx, index, guard.path(), url).await;
    guard.cleanup().await;
    res
}

async fn download_into_stage_and_finalize(
    ctx: &DownloadContext,
    index: usize,
    stage_dir: &Path,
    url: &str,
) -> Result<Vec<PathBuf>> {
    let args = build_yt_dlp_args(
        ctx.format,
        ctx.playlist,
        stage_dir,
        ctx.ffmpeg_location.as_deref(),
        ctx.js_runtime.as_deref(),
        ctx.embed_thumbnail,
        url,
    );
    let staged_files = execute_yt_dlp(
        &ctx.yt_dlp_bin,
        &args,
        stage_dir,
        &ctx.env,
        index,
        ctx.progress_tx.as_ref(),
    )
    .await?;
    if staged_files.is_empty() {
        return Ok(Vec::new());
    }

    ctx.emit(DownloadProgressEvent::ItemUpdate {
        index,
        title: None,
        phase: DownloadPhase::Finalizing,
    });
    let _finalize_guard = ctx.finalizer_lock.lock().await;
    let dest_dir = ctx.dest_dir.clone();
    run_blocking(move || {
        let mut final_paths = Vec::with_capacity(staged_files.len());
        for staged in staged_files {
            final_paths.push(move_staged_file(&staged, &dest_dir)?);
        }
        Ok(final_paths)
    })
    .await
}

fn validate_non_empty_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().to_string_lossy().trim().is_empty() {
        return Err(TermusicError::InvalidArg(format!(
            "{label} path cannot be empty"
        )));
    }
    Ok(())
}

/// Validate and prepare the effective download destination for `termusic search -d`
/// (`dest` -> `TERMUSIC_DEST` -> `WorkspaceConfig.download_dest`).
pub fn validate_search_download_destination(
    paths: &AppPaths,
    dest: Option<&Path>,
) -> Result<PathBuf> {
    let env = DownloadEnv::from_system();
    resolve_destination_with_env(paths, dest, None, &env)
}

fn resolve_destination(
    paths: &AppPaths,
    opts: &DownloadOptions,
    saved_dest: Option<&Path>,
    env: &DownloadEnv,
) -> Result<PathBuf> {
    resolve_destination_with_env(paths, opts.dest.as_deref(), saved_dest, env)
}

fn resolve_destination_with_env(
    paths: &AppPaths,
    dest_opt: Option<&Path>,
    saved_dest: Option<&Path>,
    env: &DownloadEnv,
) -> Result<PathBuf> {
    let raw_dest: PathBuf = if let Some(dest) = dest_opt {
        validate_non_empty_path(dest, "--dest")?;
        dest.to_path_buf()
    } else if let Some(saved) = saved_dest {
        saved.to_path_buf()
    } else if let Some(env_dest) = env
        .termusic_dest
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        PathBuf::from(env_dest)
    } else {
        let cfg = load_workspace_config(paths)?;
        match cfg.download_dest {
            Some(dest) => {
                validate_non_empty_path(&dest, "configured download_dest")?;
                dest
            }
            None => {
                return Err(TermusicError::InvalidArg(
                    "no download destination configured; pass --dest, --set-dest, or set TERMUSIC_DEST"
                        .into(),
                ));
            }
        }
    };

    prepare_directory(&raw_dest)
}

fn prepare_directory(dir: &Path) -> Result<PathBuf> {
    if dir.exists() && !dir.is_dir() {
        return Err(TermusicError::InvalidArg(format!(
            "download destination '{}' exists and is not a directory",
            dir.display()
        )));
    }
    fs::create_dir_all(dir)?;
    Ok(canonicalize_clean(dir)?)
}

fn find_in_env(binary: &str, env: &DownloadEnv) -> Option<PathBuf> {
    ytdlp::find_in_env(binary, &env.to_tool_env())
}

#[cfg(test)]
fn append_windows_quoted_arg(out: &mut OsString, arg: &OsStr) {
    let text = arg.to_string_lossy();
    let needs_quotes = text.is_empty()
        || text.chars().any(|ch| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '"' | '&' | '|' | '<' | '>' | '(' | ')' | '^' | ',' | ';' | '=' | '%' | '!'
                )
        });
    if !needs_quotes {
        out.push(arg);
        return;
    }

    out.push("\"");
    let mut backslashes = 0usize;
    for ch in text.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push("\\");
                }
                backslashes = 0;
                out.push("\"");
            }
            _ => {
                for _ in 0..backslashes {
                    out.push("\\");
                }
                backslashes = 0;
                let mut buf = [0u8; 4];
                out.push(ch.encode_utf8(&mut buf));
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push("\\");
    }
    out.push("\"");
}

/// Format a `.cmd`/`.bat` invocation wrapped in outer `"..."` for `cmd.exe /D /S /C`.
#[cfg(test)]
fn format_windows_cmd_batch_line(binary: &Path, args: &[OsString]) -> OsString {
    let mut line = OsString::from("\"");
    append_windows_quoted_arg(&mut line, binary.as_os_str());
    for arg in args {
        line.push(" ");
        append_windows_quoted_arg(&mut line, arg);
    }
    line.push("\"");
    line
}

fn command_for_binary(binary: &Path, args: &[OsString]) -> Command {
    ytdlp::command_for_binary(binary, args)
}

fn find_yt_dlp(env: &DownloadEnv) -> Result<PathBuf> {
    ytdlp::find_yt_dlp(&env.to_tool_env())
}

fn find_ffmpeg(env: &DownloadEnv) -> Result<PathBuf> {
    ytdlp::find_ffmpeg(&env.to_tool_env()).ok_or_else(|| {
        #[cfg(target_os = "android")]
        let hint = "neither ffmpeg nor avconv found in PATH (install via: pkg install ffmpeg)";
        #[cfg(target_os = "macos")]
        let hint = "neither ffmpeg nor avconv found in PATH or Homebrew (install via: brew install ffmpeg)";
        #[cfg(windows)]
        let hint = "neither ffmpeg nor avconv found in PATH or WinGet/Scoop (install via: winget install Gyan.FFmpeg)";
        #[cfg(not(any(target_os = "android", target_os = "macos", windows)))]
        let hint = "neither ffmpeg nor avconv found in PATH";

        TermusicError::NotFound(hint.into())
    })
}

fn detect_js_runtime(env: &DownloadEnv) -> Option<String> {
    ytdlp::detect_js_runtime(&env.to_tool_env())
}

async fn has_python_mutagen(env: &DownloadEnv) -> bool {
    let Some(python) = find_in_env("python3", env).or_else(|| find_in_env("python", env)) else {
        return false;
    };

    let mut cmd = command_for_binary(&python, &[OsString::from("-c"), OsString::from("import mutagen")]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path_var) = env.path_var.as_deref() {
        cmd.env("PATH", path_var);
    }

    cmd.status().await.map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
async fn should_embed_thumbnail(opts: &DownloadOptions, env: &DownloadEnv) -> bool {
    should_embed_thumbnail_with_progress(opts, env, None).await
}

async fn should_embed_thumbnail_with_progress(
    opts: &DownloadOptions,
    env: &DownloadEnv,
    progress_tx: Option<&mpsc::UnboundedSender<DownloadProgressEvent>>,
) -> bool {
    if !opts.embed_thumbnail {
        return false;
    }

    match opts.format {
        DownloadFormat::Wav | DownloadFormat::Aac => {
            let msg = format!(
                "skipping thumbnail embedding for {} format",
                opts.format
            );
            if let Some(tx) = progress_tx {
                let _ = tx.send(DownloadProgressEvent::Notice(msg));
            } else {
                eprintln!("note: {msg}");
            }
            false
        }
        DownloadFormat::Ogg => {
            if has_python_mutagen(env).await {
                true
            } else {
                let msg = "skipping thumbnail embedding for ogg (python3 mutagen not available)"
                    .to_string();
                if let Some(tx) = progress_tx {
                    let _ = tx.send(DownloadProgressEvent::Notice(msg));
                } else {
                    eprintln!("note: {msg}");
                }
                false
            }
        }
        DownloadFormat::Mp3 | DownloadFormat::Flac | DownloadFormat::M4a | DownloadFormat::Mp4 => {
            true
        }
    }
}

fn build_yt_dlp_args(
    format: DownloadFormat,
    playlist: bool,
    stage_dir: &Path,
    ffmpeg_location: Option<&Path>,
    js_runtime: Option<&str>,
    embed_thumbnail: bool,
    url: &str,
) -> Vec<OsString> {
    let mut temp_paths = OsString::from("temp:");
    temp_paths.push(stage_dir.as_os_str());

    let mut args = if format.is_audio() {
        vec![
            OsString::from("-f"),
            OsString::from("bestaudio/best"),
            OsString::from("-x"),
            OsString::from("--audio-format"),
            OsString::from(format.yt_dlp_format()),
            OsString::from("--audio-quality"),
            OsString::from("0"),
            OsString::from("--add-metadata"),
        ]
    } else {
        vec![
            OsString::from("-t"),
            OsString::from("mp4"),
            OsString::from("--add-metadata"),
        ]
    };

    if embed_thumbnail {
        args.push(OsString::from("--embed-thumbnail"));
    }

    args.push(OsString::from(if playlist {
        "--yes-playlist"
    } else {
        "--no-playlist"
    }));

    if let Some(ffmpeg_dir) = ffmpeg_location {
        args.push(OsString::from("--ffmpeg-location"));
        args.push(ffmpeg_dir.as_os_str().to_os_string());
    }

    if let Some(runtime) = js_runtime {
        args.push(OsString::from("--js-runtimes"));
        args.push(OsString::from(runtime));
    }

    #[cfg(any(windows, target_os = "android"))]
    {
        args.push(OsString::from("--windows-filenames"));
        args.push(OsString::from("--trim-filenames"));
        args.push(OsString::from("180"));
    }

    args.push(OsString::from("--newline"));
    args.push(OsString::from("--progress"));
    args.push(OsString::from("--progress-template"));
    args.push(OsString::from(PROGRESS_TEMPLATE_DOWNLOAD));
    args.push(OsString::from("--progress-template"));
    args.push(OsString::from(PROGRESS_TEMPLATE_POSTPROCESS));
    args.push(OsString::from("--paths"));
    args.push(temp_paths);
    args.push(OsString::from("-P"));
    args.push(stage_dir.as_os_str().to_os_string());
    args.push(OsString::from("-o"));
    args.push(OsString::from("%(title)s.%(ext)s"));
    args.push(OsString::from("--print"));
    args.push(OsString::from("after_move:filepath"));
    args.push(OsString::from("--"));
    args.push(OsString::from(url));

    args
}

async fn read_stream_lines<R, F>(mut reader: R, mut on_line: F) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    F: FnMut(String),
{
    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::with_capacity(512);
    const MAX_LINE_BYTES: usize = 64 * 1024;

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            if byte == b'\n' || byte == b'\r' {
                if !pending.is_empty() {
                    let line = String::from_utf8_lossy(&pending).into_owned();
                    pending.clear();
                    on_line(line);
                }
            } else if pending.len() < MAX_LINE_BYTES {
                pending.push(byte);
            }
        }
    }

    if !pending.is_empty() {
        on_line(String::from_utf8_lossy(&pending).into_owned());
    }
    Ok(())
}

fn clean_template_field(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "NA" || trimmed == "None" {
        None
    } else {
        Some(trimmed)
    }
}

fn parse_template_u64(raw: &str) -> Option<u64> {
    let trimmed = clean_template_field(raw)?;
    if let Ok(v) = trimmed.parse::<u64>() {
        return Some(v);
    }
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v.round() as u64)
}

fn friendly_postprocessor_name(raw: &str) -> String {
    match raw.trim() {
        "ExtractAudio" => "Extracting audio".to_string(),
        "Metadata" | "FFmpegMetadata" => "Writing metadata".to_string(),
        "EmbedThumbnail" => "Embedding artwork".to_string(),
        "MoveFiles" => "Finalizing file".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "Post-processing".to_string(),
    }
}

fn parse_yt_dlp_progress_line(line: &str, index: usize) -> Option<DownloadProgressEvent> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("TERMUSIC_PROG\t") {
        let mut parts = rest.splitn(7, '\t');
        let status = parts.next()?.trim();
        let downloaded_s = parts.next().unwrap_or("");
        let total_s = parts.next().unwrap_or("");
        let estimate_s = parts.next().unwrap_or("");
        let speed_s = parts.next().unwrap_or("");
        let eta_s = parts.next().unwrap_or("");
        let title_s = parts.next().unwrap_or("");

        let title = clean_template_field(title_s).map(str::to_string);
        let phase = if status == "finished" {
            DownloadPhase::PostProcessing {
                stage: "Finishing download".to_string(),
            }
        } else {
            DownloadPhase::Downloading {
                downloaded_bytes: parse_template_u64(downloaded_s),
                total_bytes: parse_template_u64(total_s).or_else(|| parse_template_u64(estimate_s)),
                speed_bytes_per_sec: parse_template_u64(speed_s),
                eta_secs: parse_template_u64(eta_s),
            }
        };
        return Some(DownloadProgressEvent::ItemUpdate {
            index,
            title,
            phase,
        });
    }

    if let Some(rest) = trimmed.strip_prefix("TERMUSIC_POST\t") {
        let mut parts = rest.splitn(3, '\t');
        let _status = parts.next()?;
        let postprocessor = parts.next().unwrap_or("");
        let title_s = parts.next().unwrap_or("");
        let title = clean_template_field(title_s).map(str::to_string);
        return Some(DownloadProgressEvent::ItemUpdate {
            index,
            title,
            phase: DownloadPhase::PostProcessing {
                stage: friendly_postprocessor_name(postprocessor),
            },
        });
    }

    None
}

fn is_yt_dlp_progress_or_status_line(trimmed: &str) -> bool {
    trimmed.starts_with("TERMUSIC_PROG\t")
        || trimmed.starts_with("TERMUSIC_POST\t")
        || trimmed.starts_with("[download]")
        || trimmed.starts_with("[ExtractAudio]")
        || trimmed.starts_with("[Metadata]")
        || trimmed.starts_with("[EmbedThumbnail]")
        || trimmed.starts_with("[MoveFiles]")
}

async fn execute_yt_dlp(
    yt_dlp_bin: &Path,
    args: &[OsString],
    stage_dir: &Path,
    env: &DownloadEnv,
    index: usize,
    progress_tx: Option<&mpsc::UnboundedSender<DownloadProgressEvent>>,
) -> Result<Vec<PathBuf>> {
    let mut cmd = command_for_binary(yt_dlp_bin, args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path_var) = env.path_var.as_deref() {
        cmd.env("PATH", path_var);
    }
    if let Some(home) = env.home_dir.as_deref() {
        cmd.env("HOME", home);
    }

    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| TermusicError::Download("yt-dlp stdout pipe missing".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| TermusicError::Download("yt-dlp stderr pipe missing".to_string()))?;

    let tx_out = progress_tx.cloned();
    let tx_err = progress_tx.cloned();

    let read_out = async move {
        let mut stdout_paths = Vec::new();
        let mut total_bytes = 0usize;
        read_stream_lines(stdout, |line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return;
            }
            if let Some(ev) = parse_yt_dlp_progress_line(trimmed, index) {
                emit_progress(tx_out.as_ref(), ev);
                return;
            }
            if is_yt_dlp_progress_or_status_line(trimmed) {
                return;
            }
            if total_bytes < MAX_CAPTURE_BYTES {
                total_bytes += trimmed.len() + 1;
                stdout_paths.push(trimmed.to_string());
            }
        })
        .await?;
        Ok::<Vec<String>, io::Error>(stdout_paths)
    };

    let read_err = async move {
        let mut stderr_lines = Vec::new();
        let mut total_bytes = 0usize;
        read_stream_lines(stderr, |line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return;
            }
            if let Some(ev) = parse_yt_dlp_progress_line(trimmed, index) {
                emit_progress(tx_err.as_ref(), ev);
                return;
            }
            if total_bytes < MAX_CAPTURE_BYTES {
                total_bytes += trimmed.len() + 1;
                stderr_lines.push(trimmed.to_string());
            } else if stderr_lines.len() >= 64 {
                stderr_lines.remove(0);
                stderr_lines.push(trimmed.to_string());
            }
        })
        .await?;
        Ok::<Vec<String>, io::Error>(stderr_lines)
    };

    let (out_res, err_res, status_res) = tokio::join!(read_out, read_err, child.wait());
    let stdout_lines = out_res?;
    let stderr_lines = err_res?;
    let status = status_res?;

    if !status.success() {
        let code_info = status
            .code()
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "terminated by signal".into());
        let detail = match extract_diagnostic_line(&stderr_lines) {
            Some(line) => format!("yt-dlp failed ({code_info}): {line}"),
            None => format!("yt-dlp failed ({code_info})"),
        };
        return Err(TermusicError::Download(detail));
    }

    Ok(resolve_printed_filepaths(&stdout_lines, stage_dir))
}

fn extract_diagnostic_line(lines: &[String]) -> Option<String> {
    ytdlp::extract_diagnostic_line(&lines.join("\n"))
}

fn resolve_printed_filepaths(lines: &[String], stage_dir: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_yt_dlp_progress_or_status_line(trimmed) {
            continue;
        }
        let raw_path = PathBuf::from(trimmed);
        let resolved = if raw_path.is_relative() {
            stage_dir.join(raw_path)
        } else {
            raw_path
        };
        if seen.insert(resolved.clone()) {
            files.push(resolved);
        }
    }

    files
}

fn disambiguated_candidate(dest_dir: &Path, file_name: &OsStr, attempt: usize) -> PathBuf {
    if attempt <= 1 {
        return dest_dir.join(file_name);
    }

    let path = Path::new(file_name);
    let mut new_name = match path.file_stem() {
        Some(stem) => stem.to_os_string(),
        None => file_name.to_os_string(),
    };
    new_name.push(format!(" ({attempt})"));
    if let Some(ext) = path.extension() {
        new_name.push(".");
        new_name.push(ext);
    }
    dest_dir.join(new_name)
}

fn is_cross_device_error(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(windows)]
    {
        const ERROR_NOT_SAME_DEVICE: i32 = 17;
        err.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = err;
        false
    }
}

fn is_hardlink_unsupported(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            err.raw_os_error(),
            Some(libc::EPERM) | Some(libc::ENOTSUP) | Some(libc::EACCES)
        )
    }
    #[cfg(windows)]
    {
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_NOT_SUPPORTED: i32 = 50;
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
        matches!(
            err.raw_os_error(),
            Some(ERROR_ACCESS_DENIED) | Some(ERROR_NOT_SUPPORTED) | Some(ERROR_PRIVILEGE_NOT_HELD)
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn retry_on_sharing_violation<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    #[cfg(windows)]
    {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        for attempt in 0..5 {
            match op() {
                Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) && attempt < 4 => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
        unreachable!("loop returns on final attempt")
    }
    #[cfg(not(windows))]
    {
        op()
    }
}

fn move_staged_file(staged_file: &Path, dest_dir: &Path) -> Result<PathBuf> {
    if !staged_file.exists() {
        return Err(TermusicError::Download(format!(
            "downloaded file missing from staging directory: {}",
            staged_file.display()
        )));
    }

    if let Some(parent) = staged_file.parent()
        && canonicalize_clean(parent).ok().as_deref() == Some(dest_dir)
    {
        return Ok(staged_file.to_path_buf());
    }

    let file_name = staged_file.file_name().ok_or_else(|| {
        TermusicError::Download(format!(
            "downloaded file has invalid filename: {}",
            staged_file.display()
        ))
    })?;

    for attempt in 1..=10_000 {
        let candidate = disambiguated_candidate(dest_dir, file_name, attempt);
        if fs::symlink_metadata(&candidate).is_ok() {
            continue;
        }

        match retry_on_sharing_violation(|| fs::hard_link(staged_file, &candidate)) {
            Ok(()) => {
                retry_on_sharing_violation(|| fs::remove_file(staged_file))?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) if is_cross_device_error(&err) => {
                copy_file_exclusively(staged_file, &candidate)?;
                retry_on_sharing_violation(|| fs::remove_file(staged_file))?;
                return Ok(candidate);
            }
            Err(err) if is_hardlink_unsupported(&err) => {
                if fs::symlink_metadata(&candidate).is_ok() {
                    continue;
                }
                match retry_on_sharing_violation(|| fs::rename(staged_file, &candidate)) {
                    Ok(()) => return Ok(candidate),
                    Err(ren_err) if ren_err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(ren_err) if is_cross_device_error(&ren_err) => {
                        copy_file_exclusively(staged_file, &candidate)?;
                        retry_on_sharing_violation(|| fs::remove_file(staged_file))?;
                        return Ok(candidate);
                    }
                    Err(ren_err) => return Err(ren_err.into()),
                }
            }
            Err(err) => return Err(err.into()),
        }
    }

    Err(TermusicError::Download(format!(
        "exhausted disambiguation attempts for {}",
        staged_file.display()
    )))
}

fn copy_file_exclusively(src: &Path, dst: &Path) -> io::Result<()> {
    let mut reader = fs::File::open(src)?;
    let mut writer = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let copy_res = (|| -> io::Result<()> {
        io::copy(&mut reader, &mut writer)?;
        writer.sync_all()?;
        Ok(())
    })();
    if let Err(err) = copy_res {
        let _ = fs::remove_file(dst);
        return Err(err);
    }
    Ok(())
}

fn index_downloaded_files(
    paths: &AppPaths,
    files: &[PathBuf],
    playlist_name: Option<&str>,
) -> Result<AddSummary> {
    Library::update(&paths.library_path, |library| {
        let mut combined = AddSummary {
            playlist_name: playlist_name.map(str::to_string),
            ..AddSummary::default()
        };

        for file in files {
            let summary = library.add_files(file, false, playlist_name)?;
            combined.added_count += summary.added_count;
            combined.existing_count += summary.existing_count;
            combined.playlist_added_count += summary.playlist_added_count;
            combined.added_titles.extend(summary.added_titles);
            combined.skipped_files.extend(summary.skipped_files);
            if combined.playlist_name.is_none() {
                combined.playlist_name = summary.playlist_name;
            }
        }

        Ok(combined)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use crate::paths::initialize_workspace_with_env;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "termusic-download-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            let canonical = canonicalize_clean(&dir).expect("canonicalize temp dir");
            Self(canonical)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TestWorkspace {
        _temp: TempDir,
        root: PathBuf,
        paths: AppPaths,
        bin_dir: PathBuf,
        home_dir: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let temp = TempDir::new();
            let root = temp.path().to_path_buf();
            let working = root.join("workspace");
            let xdg_config = root.join("xdg_config");
            let runtime = root.join("runtime");
            let bin_dir = root.join("bin");
            let home_dir = root.join("home");
            fs::create_dir_all(&working).expect("create working dir");
            fs::create_dir_all(&xdg_config).expect("create xdg_config dir");
            fs::create_dir_all(&runtime).expect("create runtime dir");
            fs::create_dir_all(&bin_dir).expect("create bin dir");
            fs::create_dir_all(&home_dir).expect("create home dir");

            initialize_workspace_with_env(
                &working,
                &root,
                Some(xdg_config.as_os_str().to_os_string()),
                Some(home_dir.clone()),
            )
            .expect("init workspace");

            let paths = AppPaths::resolve_from_env(
                Some(xdg_config.into_os_string()),
                Some(home_dir.clone()),
                Some(runtime.into_os_string()),
                "1000",
            )
            .expect("resolve paths");

            Self {
                _temp: temp,
                root,
                paths,
                bin_dir,
                home_dir,
            }
        }

        fn env(&self, termusic_dest: Option<&str>) -> DownloadEnv {
            DownloadEnv {
                home_dir: Some(self.home_dir.clone()),
                path_var: Some(self.bin_dir.as_os_str().to_os_string()),
                termusic_dest: termusic_dest.map(str::to_string),
                pathext: None,
                extra_search_dirs: Vec::new(),
            }
        }
    }

    fn mock_bin_path(dir: &Path, name: &str) -> PathBuf {
        #[cfg(windows)]
        {
            dir.join(format!("{name}.cmd"))
        }
        #[cfg(not(windows))]
        {
            dir.join(name)
        }
    }

    fn write_executable(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create script parent");
        }
        fs::write(path, content).expect("write script");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("chmod");
        }
    }

    fn write_noop_executable(dir: &Path, name: &str) -> PathBuf {
        let path = mock_bin_path(dir, name);
        #[cfg(windows)]
        let script = "@echo off\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        let script = "#!/bin/sh\nexit 0\n";
        write_executable(&path, script);
        path
    }

    fn write_failing_yt_dlp(dir: &Path, exit_code: i32, stderr_msg: &str) {
        let path = mock_bin_path(dir, "yt-dlp");
        #[cfg(windows)]
        let script = format!("@echo off\r\n>&2 echo {stderr_msg}\r\nexit /b {exit_code}\r\n");
        #[cfg(not(windows))]
        let script = format!("#!/bin/sh\necho '{stderr_msg}' >&2\nexit {exit_code}\n");
        write_executable(&path, &script);
    }

    #[allow(dead_code)]
    fn js_escape(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    fn write_mock_yt_dlp(bin_dir: &Path, unix_body: &str, win_js_body: &str) {
        #[cfg(unix)]
        {
            let _ = win_js_body;
            let script = format!(
                "#!/bin/sh\nset -eu\nPATH=\"/bin:/usr/bin:${{PREFIX:-/usr}}/bin:$PATH\"\nstage=\"\"\nprev=\"\"\nlast=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-P\" ]; then\n    stage=\"$a\"\n  fi\n  prev=\"$a\"\n  last=\"$a\"\ndone\n{unix_body}\n"
            );
            write_executable(&bin_dir.join("yt-dlp"), &script);
        }
        #[cfg(windows)]
        {
            let _ = unix_body;
            let js_path = bin_dir.join("yt-dlp-mock.js");
            let js_script = format!(
                "var fso = new ActiveXObject(\"Scripting.FileSystemObject\");\n\
                 var allArgs = [];\n\
                 var stage = \"\";\n\
                 var prev = \"\";\n\
                 var last = \"\";\n\
                 for (var i = 0; i < WScript.Arguments.Length; i++) {{\n\
                   var a = WScript.Arguments(i);\n\
                   allArgs.push(a);\n\
                   if (prev === \"-P\") stage = a;\n\
                   prev = a;\n\
                   last = a;\n\
                 }}\n\
                 {win_js_body}\n"
            );
            fs::write(&js_path, js_script).expect("write js mock");
            write_executable(
                &bin_dir.join("yt-dlp.cmd"),
                "@\"%SystemRoot%\\System32\\cscript.exe\" //Nologo //E:JScript \"%~dp0yt-dlp-mock.js\" %*\r\n",
            );
        }
    }

    fn write_synthetic_wav(path: &Path, duration_secs: f32) {
        let sample_rate: u32 = 22_050;
        let channels: u16 = 1;
        let bits_per_sample: u16 = 16;
        let num_samples = (sample_rate as f32 * duration_secs) as u32;
        let data_bytes = num_samples * u32::from(channels) * u32::from(bits_per_sample / 8);
        let riff_size = 36 + data_bytes;

        let mut buf = Vec::with_capacity((riff_size + 8) as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        let block_align = channels * (bits_per_sample / 8);
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());
        for i in 0..num_samples {
            let sample = ((i as f32 * 0.05).sin() * 4000.0) as i16;
            buf.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(path, buf).expect("write wav");
    }

    fn base_options(urls: Vec<&str>) -> DownloadOptions {
        DownloadOptions {
            urls: urls.into_iter().map(str::to_string).collect(),
            dest: None,
            set_dest: None,
            format: DownloadFormat::Mp3,
            playlist: false,
            embed_thumbnail: true,
            auto_add: true,
            playlist_name: None,
            jobs: 4,
            no_tui: false,
        }
    }

    /// Generate a WAV payload using the test binary helper script (or copy prebuilt template).
    /// Note: in shell mock scripts we copy a pre-generated valid WAV template file from `$root/template.wav`
    /// so every concurrent mock run finishes cleanly without needing python/sox inside shell.
    fn write_wav_template(path: &Path) -> PathBuf {
        write_synthetic_wav(path, 0.25);
        path.to_path_buf()
    }

    #[tokio::test]
    async fn missing_urls_without_set_dest_fails() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let opts = base_options(vec![]);

        let err = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .unwrap_err();
        assert!(matches!(err, TermusicError::InvalidArg(_)));
    }

    #[tokio::test]
    async fn empty_dest_or_set_dest_is_rejected() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);

        let mut opts = base_options(vec!["https://example.com/a"]);
        opts.dest = Some(PathBuf::from("   "));
        assert!(matches!(
            run_download_with_env(&ws.paths, &opts, &env)
                .await
                .unwrap_err(),
            TermusicError::InvalidArg(_)
        ));

        let mut set_only = base_options(vec![]);
        set_only.set_dest = Some(PathBuf::from(""));
        assert!(matches!(
            run_download_with_env(&ws.paths, &set_only, &env)
                .await
                .unwrap_err(),
            TermusicError::InvalidArg(_)
        ));
    }

    #[tokio::test]
    async fn set_dest_without_urls_persists_canonical_destination() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let target = ws.root.join("my_downloads");

        let mut opts = base_options(vec![]);
        opts.set_dest = Some(target.clone());

        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("set_dest");
        let canonical = canonicalize_clean(&target).expect("canonical");
        assert_eq!(outcome.saved_dest.as_ref(), Some(&canonical));
        assert!(outcome.downloaded_files.is_empty());
        assert!(outcome.failed.is_empty());
        assert!(outcome.add_summary.is_none());

        let cfg = load_workspace_config(&ws.paths).expect("reload config");
        assert_eq!(cfg.download_dest.as_ref(), Some(&canonical));
    }

    #[test]
    fn destination_precedence_and_file_rejection() {
        let ws = TestWorkspace::new();
        let env_dir = ws.root.join("from_env");
        let config_dir = ws.root.join("from_config");
        let set_dir = ws.root.join("from_set_dest");
        let flag_dir = ws.root.join("from_dest_flag");

        set_workspace_download_dest(&ws.paths, &config_dir).expect("save config dest");
        let env_with_var = ws.env(Some(env_dir.to_str().unwrap()));
        let env_without_var = ws.env(None);

        // 4th fallback: configured download_dest
        let opts = base_options(vec!["https://example.com/x"]);
        assert_eq!(
            resolve_destination(&ws.paths, &opts, None, &env_without_var).unwrap(),
            canonicalize_clean(&config_dir).unwrap()
        );

        // 3rd: TERMUSIC_DEST overrides configured download_dest
        assert_eq!(
            resolve_destination(&ws.paths, &opts, None, &env_with_var).unwrap(),
            canonicalize_clean(&env_dir).unwrap()
        );

        // 2nd: saved_dest (--set-dest) overrides TERMUSIC_DEST
        fs::create_dir_all(&set_dir).unwrap();
        let canonical_set = canonicalize_clean(&set_dir).unwrap();
        assert_eq!(
            resolve_destination(&ws.paths, &opts, Some(&canonical_set), &env_with_var).unwrap(),
            canonical_set
        );

        // 1st: --dest overrides --set-dest and TERMUSIC_DEST
        let mut flag_opts = opts.clone();
        flag_opts.dest = Some(flag_dir.clone());
        assert_eq!(
            resolve_destination(&ws.paths, &flag_opts, Some(&canonical_set), &env_with_var)
                .unwrap(),
            canonicalize_clean(&flag_dir).unwrap()
        );

        // Existing non-directory file is rejected
        let file_dest = ws.root.join("not_a_dir");
        fs::write(&file_dest, b"data").unwrap();
        flag_opts.dest = Some(file_dest);
        let err = resolve_destination(&ws.paths, &flag_opts, None, &env_without_var).unwrap_err();
        assert!(matches!(err, TermusicError::InvalidArg(_)));
    }

    #[test]
    fn binary_discovery_and_js_runtime_precedence() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);

        assert!(matches!(find_yt_dlp(&env), Err(TermusicError::NotFound(_))));
        assert!(matches!(find_ffmpeg(&env), Err(TermusicError::NotFound(_))));
        assert_eq!(detect_js_runtime(&env), None);

        // PATH yt-dlp, overridden when ~/.local/bin/yt-dlp exists
        let path_yt = write_noop_executable(&ws.bin_dir, "yt-dlp");
        assert_eq!(find_yt_dlp(&env).unwrap(), path_yt);

        let local_yt = write_noop_executable(&ws.home_dir.join(".local/bin"), "yt-dlp");
        assert_eq!(find_yt_dlp(&env).unwrap(), local_yt);

        // avconv fallback when ffmpeg absent, ffmpeg preferred when present
        let avconv = write_noop_executable(&ws.bin_dir, "avconv");
        assert_eq!(find_ffmpeg(&env).unwrap(), avconv);

        let ffmpeg = write_noop_executable(&ws.bin_dir, "ffmpeg");
        assert_eq!(find_ffmpeg(&env).unwrap(), ffmpeg);

        // extra_search_dirs fallback when binary is not on PATH
        let fallback_dir = ws.root.join("fallback_bin");
        let extra_node = write_noop_executable(&fallback_dir, "node");
        let mut env_extra = env.clone();
        env_extra.extra_search_dirs = vec![fallback_dir];
        assert_eq!(
            detect_js_runtime(&env_extra).as_deref(),
            Some(format!("node:{}", extra_node.display()).as_str())
        );

        // JS runtime priority: extra_dir < PATH node < PATH deno
        let node = write_noop_executable(&ws.bin_dir, "node");
        assert_eq!(
            detect_js_runtime(&env_extra).as_deref(),
            Some(format!("node:{}", node.display()).as_str())
        );

        let deno = write_noop_executable(&ws.bin_dir, "deno");
        assert_eq!(
            detect_js_runtime(&env_extra).as_deref(),
            Some(format!("deno:{}", deno.display()).as_str())
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pathext_prefers_exe_over_cmd_shim() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let cmd_path = ws.bin_dir.join("yt-dlp.cmd");
        let exe_path = ws.bin_dir.join("yt-dlp.exe");
        write_executable(&cmd_path, "@echo off\r\nexit /b 0\r\n");
        assert_eq!(find_yt_dlp(&env).unwrap(), cmd_path);
        write_executable(&exe_path, "");
        assert_eq!(find_yt_dlp(&env).unwrap(), exe_path);
    }

    #[tokio::test]
    async fn argument_mapping_and_thumbnail_guards() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let stage = ws.root.join("stage");
        fs::create_dir_all(&stage).unwrap();

        // wav and aac never embed thumbnails; ogg checks python3/python mutagen
        let mut wav_opts = base_options(vec!["https://example.com/1"]);
        wav_opts.format = DownloadFormat::Wav;
        assert!(!should_embed_thumbnail(&wav_opts, &env).await);

        let mut aac_opts = wav_opts.clone();
        aac_opts.format = DownloadFormat::Aac;
        assert!(!should_embed_thumbnail(&aac_opts, &env).await);

        let mut ogg_opts = wav_opts.clone();
        ogg_opts.format = DownloadFormat::Ogg;
        assert!(!should_embed_thumbnail(&ogg_opts, &env).await);

        // Mock python3 returning success for `import mutagen`
        write_noop_executable(&ws.bin_dir, "python3");
        assert!(should_embed_thumbnail(&ogg_opts, &env).await);

        let args = build_yt_dlp_args(
            ogg_opts.format,
            ogg_opts.playlist,
            &stage,
            Some(&ws.bin_dir),
            Some("node"),
            false,
            "https://example.com/1",
        );
        let str_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(str_args.windows(2).any(|w| w == ["--audio-format", "vorbis"]));
        assert!(str_args.contains(&"--no-playlist".to_string()));
        assert!(str_args.contains(&"--newline".to_string()));
        assert!(str_args.contains(&"--progress".to_string()));
        assert!(!str_args.contains(&"--embed-thumbnail".to_string()));
        assert!(
            str_args
                .windows(2)
                .any(|w| w == ["--ffmpeg-location", &ws.bin_dir.display().to_string()])
        );
        assert!(str_args.windows(2).any(|w| w == ["--js-runtimes", "node"]));
        #[cfg(any(windows, target_os = "android"))]
        {
            assert!(str_args.contains(&"--windows-filenames".to_string()));
            assert!(str_args.windows(2).any(|w| w == ["--trim-filenames", "180"]));
        }
        assert!(
            str_args
                .windows(2)
                .any(|w| w == ["--paths", &format!("temp:{}", stage.display())])
        );
        assert!(
            str_args
                .windows(2)
                .any(|w| w == ["-P", &stage.display().to_string()])
        );
        assert!(
            str_args
                .windows(2)
                .any(|w| w == ["--print", "after_move:filepath"])
        );
        assert_eq!(
            &str_args[str_args.len() - 2..],
            ["--", "https://example.com/1"]
        );

        let pl_args: Vec<String> = build_yt_dlp_args(
            DownloadFormat::Mp3,
            true,
            &stage,
            None,
            None,
            true,
            "https://example.com/list",
        )
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert!(pl_args.contains(&"--yes-playlist".to_string()));
        assert!(pl_args.contains(&"--embed-thumbnail".to_string()));
        assert!(!pl_args.contains(&"--ffmpeg-location".to_string()));
        assert!(!pl_args.contains(&"--js-runtimes".to_string()));
    }

    #[tokio::test]
    async fn mock_yt_dlp_downloads_wav_and_respects_auto_add_and_playlist() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        fs::create_dir_all(&dest).unwrap();
        let template_wav = write_wav_template(&ws.root.join("template.wav"));

        write_noop_executable(&ws.bin_dir, "ffmpeg");

        let args_log = ws.root.join("yt-dlp-args.txt");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                "printf '%s\\n' \"$@\" > \"{args_log}\"\nout=\"$stage/Synthetic Track.wav\"\ncp \"{template}\" \"$out\"\nprintf '%s\\n' \"$out\"",
                args_log = args_log.display(),
                template = template_wav.display(),
            ),
            &format!(
                "var ts = fso.OpenTextFile(\"{args_log}\", 2, true);\n\
                 for (var i = 0; i < allArgs.length; i++) ts.WriteLine(allArgs[i]);\n\
                 ts.Close();\n\
                 var out = stage + \"\\\\Synthetic Track.wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                args_log = js_escape(&args_log),
                template = js_escape(&template_wav),
            ),
        );

        let env = ws.env(None);
        let expected_dest_track = canonicalize_clean(&dest).unwrap().join("Synthetic Track.wav");

        // First run with auto_add = false moves staged track into dest but does not touch library.json
        let mut no_add_opts = base_options(vec!["https://example.com/track"]);
        no_add_opts.dest = Some(dest.clone());
        no_add_opts.format = DownloadFormat::Wav;
        no_add_opts.auto_add = false;

        let no_add_out = run_download_with_env(&ws.paths, &no_add_opts, &env)
            .await
            .expect("no-add run");
        assert!(no_add_out.failed.is_empty(), "failures: {:?}", no_add_out.failed);
        assert_eq!(no_add_out.downloaded_files, vec![expected_dest_track.clone()]);
        assert!(expected_dest_track.exists());
        assert!(!dest.join(".termusic-dl-tmp").exists());
        assert!(no_add_out.add_summary.is_none());
        let lib_before = Library::load(&ws.paths.library_path).expect("load empty lib");
        assert!(lib_before.songs.is_empty());

        let logged_args = fs::read_to_string(&args_log).expect("read logged args");
        assert!(logged_args.lines().any(|l| l == "wav"));
        assert!(!logged_args.lines().any(|l| l == "--embed-thumbnail"));
        assert!(logged_args.lines().any(|l| l == "--no-playlist"));

        // Second run with auto_add = true and playlist_name creates song + playlist (and disambiguates collision)
        let mut add_opts = base_options(vec!["https://example.com/track"]);
        add_opts.dest = Some(dest.clone());
        add_opts.format = DownloadFormat::Wav;
        add_opts.playlist = true;
        add_opts.playlist_name = Some("Downloaded".into());

        let add_out = run_download_with_env(&ws.paths, &add_opts, &env)
            .await
            .expect("auto-add run");
        let second_expected = canonicalize_clean(&dest)
            .unwrap()
            .join("Synthetic Track (2).wav");
        assert_eq!(add_out.downloaded_files, vec![second_expected.clone()]);
        let summary = add_out.add_summary.expect("add summary");
        assert_eq!(summary.added_count, 1);
        assert_eq!(summary.playlist_name.as_deref(), Some("Downloaded"));
        assert_eq!(summary.playlist_added_count, 1);

        let lib_after = Library::load(&ws.paths.library_path).expect("load indexed lib");
        assert_eq!(lib_after.songs.len(), 1);
        assert_eq!(lib_after.songs[0].path, second_expected);
        assert_eq!(lib_after.playlists.len(), 1);
        assert_eq!(lib_after.playlists[0].name, "Downloaded");
        assert_eq!(lib_after.playlists[0].song_ids.len(), 1);

        let logged_args_2 = fs::read_to_string(&args_log).expect("read logged args 2");
        assert!(logged_args_2.lines().any(|l| l == "--yes-playlist"));
    }

    #[tokio::test]
    async fn yt_dlp_non_zero_exit_records_failure_in_outcome() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        write_noop_executable(&ws.bin_dir, "ffmpeg");
        write_failing_yt_dlp(&ws.bin_dir, 2, "ERROR: video unavailable");

        let env = ws.env(None);
        let mut opts = base_options(vec!["https://example.com/fail"]);
        opts.dest = Some(dest.clone());

        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("per-URL error recorded in outcome");
        assert!(outcome.downloaded_files.is_empty());
        assert!(outcome.add_summary.is_none());
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].url, "https://example.com/fail");
        assert!(outcome.failed[0].error.contains("exit code 2"));
        assert!(outcome.failed[0].error.contains("ERROR: video unavailable"));
        assert!(!dest.join(".termusic-dl-tmp").exists());
    }

    #[tokio::test]
    async fn parallel_downloads_overlap_and_preserve_url_order() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        let template_wav = write_wav_template(&ws.root.join("template.wav"));
        let active_dir = ws.root.join("active_workers");
        fs::create_dir_all(&active_dir).unwrap();

        write_noop_executable(&ws.bin_dir, "ffmpeg");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                r#"name=$(printf '%s' "$last" | awk -F/ '{{print $NF}}')
marker="{active}/running.$name"
: > "$marker"
sleep 0.20
count=$(find "{active}" -maxdepth 1 -name 'running.*' | wc -l)
printf '%s\n' "$count" > "{active}/$name.peak"
rm -f "$marker"
out="$stage/$name.wav"
cp "{template}" "$out"
printf '%s\n' "$out""#,
                active = active_dir.display(),
                template = template_wav.display(),
            ),
            &format!(
                "var slash = last.lastIndexOf(\"/\");\n\
                 var name = slash >= 0 ? last.slice(slash + 1) : last;\n\
                 var marker = \"{active}\\\\running.\" + name;\n\
                 var mf = fso.CreateTextFile(marker, true);\n\
                 mf.Close();\n\
                 WScript.Sleep(350);\n\
                 var folder = fso.GetFolder(\"{active}\");\n\
                 var cnt = 0;\n\
                 for (var e = new Enumerator(folder.Files); !e.atEnd(); e.moveNext()) {{\n\
                   if (e.item().Name.indexOf(\"running.\") === 0) cnt++;\n\
                 }}\n\
                 var pf = fso.CreateTextFile(\"{active}\\\\\" + name + \".peak\", true);\n\
                 pf.WriteLine(\"\" + cnt);\n\
                 pf.Close();\n\
                 fso.DeleteFile(marker, true);\n\
                 var out = stage + \"\\\\\" + name + \".wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                active = js_escape(&active_dir),
                template = js_escape(&template_wav),
            ),
        );

        let env = ws.env(None);
        let mut opts = base_options(vec![
            "https://example.com/alpha",
            "https://example.com/beta",
            "https://example.com/gamma",
        ]);
        opts.dest = Some(dest.clone());
        opts.format = DownloadFormat::Wav;
        opts.jobs = 3;

        let started = std::time::Instant::now();
        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("parallel download succeeds");
        let elapsed = started.elapsed();

        assert!(outcome.failed.is_empty(), "failures: {:?}", outcome.failed);
        assert_eq!(outcome.downloaded_files.len(), 3);
        let names: Vec<String> = outcome
            .downloaded_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["alpha.wav", "beta.wav", "gamma.wav"]);
        #[cfg(windows)]
        let max_elapsed = std::time::Duration::from_millis(950);
        #[cfg(not(windows))]
        let max_elapsed = std::time::Duration::from_millis(520);
        assert!(
            elapsed < max_elapsed,
            "expected concurrent overlap faster than sequential runs, took {elapsed:?}"
        );
        let max_peak: usize = fs::read_dir(&active_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("peak"))
            .filter_map(|p| fs::read_to_string(p).ok())
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        assert!(max_peak >= 2, "expected overlapping workers, max_peak={max_peak}");
    }

    #[tokio::test]
    async fn jobs_one_bounds_concurrency_to_single_worker() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        let template_wav = write_wav_template(&ws.root.join("template.wav"));
        let mutex_dir = ws.root.join("mutex.lock");

        write_noop_executable(&ws.bin_dir, "ffmpeg");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                r#"if ! mkdir "{mutex}" 2>/dev/null; then
  echo "ERROR: concurrent execution exceeded jobs=1" >&2
  exit 99
fi
sleep 0.04
name=$(printf '%s' "$last" | awk -F/ '{{print $NF}}')
out="$stage/$name.wav"
cp "{template}" "$out"
rmdir "{mutex}"
printf '%s\n' "$out""#,
                mutex = mutex_dir.display(),
                template = template_wav.display(),
            ),
            &format!(
                "try {{\n\
                   fso.CreateFolder(\"{mutex}\");\n\
                 }} catch (e) {{\n\
                   WScript.StdErr.WriteLine(\"ERROR: concurrent execution exceeded jobs=1\");\n\
                   WScript.Quit(99);\n\
                 }}\n\
                 WScript.Sleep(40);\n\
                 var slash = last.lastIndexOf(\"/\");\n\
                 var name = slash >= 0 ? last.slice(slash + 1) : last;\n\
                 var out = stage + \"\\\\\" + name + \".wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 fso.DeleteFolder(\"{mutex}\", true);\n\
                 WScript.StdOut.WriteLine(out);",
                mutex = js_escape(&mutex_dir),
                template = js_escape(&template_wav),
            ),
        );

        let env = ws.env(None);
        let mut opts = base_options(vec![
            "https://example.com/one",
            "https://example.com/two",
            "https://example.com/three",
        ]);
        opts.dest = Some(dest);
        opts.format = DownloadFormat::Wav;
        opts.jobs = 1;

        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("jobs=1 succeeds");
        assert!(outcome.failed.is_empty(), "failures: {:?}", outcome.failed);
        assert_eq!(outcome.downloaded_files.len(), 3);
    }

    #[tokio::test]
    async fn partial_failure_indexes_healthy_urls_and_reports_failed_url() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        let template_wav = write_wav_template(&ws.root.join("template.wav"));

        write_noop_executable(&ws.bin_dir, "ffmpeg");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                r#"case "$last" in
  *broken*)
    echo "ERROR: broken stream" >&2
    exit 3
    ;;
esac
name=$(printf '%s' "$last" | awk -F/ '{{print $NF}}')
out="$stage/$name.wav"
cp "{template}" "$out"
printf '%s\n' "$out""#,
                template = template_wav.display(),
            ),
            &format!(
                "if (last.indexOf(\"broken\") >= 0) {{\n\
                   WScript.StdErr.WriteLine(\"ERROR: broken stream\");\n\
                   WScript.Quit(3);\n\
                 }}\n\
                 var slash = last.lastIndexOf(\"/\");\n\
                 var name = slash >= 0 ? last.slice(slash + 1) : last;\n\
                 var out = stage + \"\\\\\" + name + \".wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                template = js_escape(&template_wav),
            ),
        );

        let env = ws.env(None);
        let mut opts = base_options(vec![
            "https://example.com/first",
            "https://example.com/broken",
            "https://example.com/third",
        ]);
        opts.dest = Some(dest);
        opts.format = DownloadFormat::Wav;
        opts.playlist_name = Some("Mixed".into());
        opts.jobs = 3;

        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("partial failure returns outcome");
        assert_eq!(outcome.downloaded_files.len(), 2);
        let titles: Vec<String> = outcome
            .downloaded_files
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(titles, vec!["first", "third"]);

        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].url, "https://example.com/broken");
        assert!(outcome.failed[0].error.contains("ERROR: broken stream"));

        let summary = outcome.add_summary.expect("healthy tracks indexed");
        assert_eq!(summary.added_count, 2);
        assert_eq!(summary.playlist_name.as_deref(), Some("Mixed"));
        assert_eq!(summary.playlist_added_count, 2);

        let lib = Library::load(&ws.paths.library_path).unwrap();
        assert_eq!(lib.songs.len(), 2);
        assert_eq!(lib.playlists.len(), 1);
        assert_eq!(lib.playlists[0].song_ids.len(), 2);
    }

    #[tokio::test]
    async fn concurrent_duplicate_titles_disambiguate_without_overwriting() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        let template_wav = write_wav_template(&ws.root.join("template.wav"));

        write_noop_executable(&ws.bin_dir, "ffmpeg");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                "out=\"$stage/Same Title.wav\"\ncp \"{}\" \"$out\"\nprintf '%s\\n' \"$out\"",
                template_wav.display(),
            ),
            &format!(
                "var out = stage + \"\\\\Same Title.wav\";\n\
                 fso.CopyFile(\"{}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                js_escape(&template_wav),
            ),
        );

        let env = ws.env(None);
        let mut opts = base_options(vec![
            "https://example.com/source1",
            "https://example.com/source2",
        ]);
        opts.dest = Some(dest.clone());
        opts.format = DownloadFormat::Wav;
        opts.jobs = 2;

        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("concurrent duplicate titles succeed");
        assert!(outcome.failed.is_empty(), "failures: {:?}", outcome.failed);
        assert_eq!(outcome.downloaded_files.len(), 2);

        let mut names: Vec<String> = outcome
            .downloaded_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Same Title (2).wav", "Same Title.wav"]);
        for path in &outcome.downloaded_files {
            assert!(path.exists(), "missing finalized track {}", path.display());
        }
        assert!(!dest.join(".termusic-dl-tmp").exists());

        let lib = Library::load(&ws.paths.library_path).unwrap();
        assert_eq!(lib.songs.len(), 2);
    }

    #[tokio::test]
    async fn whitespace_split_and_deduplicated_urls_execute_once_each() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        let template_wav = write_wav_template(&ws.root.join("template.wav"));
        let invocation_log = ws.root.join("invocations.log");

        write_noop_executable(&ws.bin_dir, "ffmpeg");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                r#"printf '%s\n' "$last" >> "{log}"
name=$(printf '%s' "$last" | awk -F/ '{{print $NF}}')
out="$stage/$name.wav"
cp "{template}" "$out"
printf '%s\n' "$out""#,
                log = invocation_log.display(),
                template = template_wav.display(),
            ),
            &format!(
                "for (var attempt = 0; attempt < 20; attempt++) {{\n\
                   try {{\n\
                     var lf = fso.OpenTextFile(\"{log}\", 8, true);\n\
                     lf.WriteLine(last);\n\
                     lf.Close();\n\
                     break;\n\
                   }} catch (e) {{ WScript.Sleep(15); }}\n\
                 }}\n\
                 var slash = last.lastIndexOf(\"/\");\n\
                 var name = slash >= 0 ? last.slice(slash + 1) : last;\n\
                 var out = stage + \"\\\\\" + name + \".wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                log = js_escape(&invocation_log),
                template = js_escape(&template_wav),
            ),
        );

        let env = ws.env(None);
        let mut opts = base_options(vec![
            "https://example.com/t1   https://example.com/t2",
            "https://example.com/t1",
            "https://example.com/t3",
        ]);
        opts.dest = Some(dest);
        opts.format = DownloadFormat::Wav;
        opts.jobs = 4;

        let outcome = run_download_with_env(&ws.paths, &opts, &env)
            .await
            .expect("split and dedup succeeds");
        assert!(outcome.failed.is_empty(), "failures: {:?}", outcome.failed);
        let names: Vec<String> = outcome
            .downloaded_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["t1.wav", "t2.wav", "t3.wav"]);

        let mut invoked: Vec<String> = fs::read_to_string(&invocation_log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        invoked.sort();
        assert_eq!(
            invoked,
            vec![
                "https://example.com/t1",
                "https://example.com/t2",
                "https://example.com/t3",
            ]
        );
    }

    #[test]
    fn parses_yt_dlp_progress_and_postprocess_templates() {
        let prog = parse_yt_dlp_progress_line(
            "TERMUSIC_PROG\tdownloading\t524288\tNA\t1048576.0\t262144.4\t2.0\tMy Cool Track",
            1,
        )
        .expect("should parse progress");
        assert_eq!(
            prog,
            DownloadProgressEvent::ItemUpdate {
                index: 1,
                title: Some("My Cool Track".to_string()),
                phase: DownloadPhase::Downloading {
                    downloaded_bytes: Some(524_288),
                    total_bytes: Some(1_048_576),
                    speed_bytes_per_sec: Some(262_144),
                    eta_secs: Some(2),
                },
            }
        );

        let post = parse_yt_dlp_progress_line(
            "TERMUSIC_POST\tstarted\tExtractAudio\tMy Cool Track",
            1,
        )
        .expect("should parse postprocess");
        assert_eq!(
            post,
            DownloadProgressEvent::ItemUpdate {
                index: 1,
                title: Some("My Cool Track".to_string()),
                phase: DownloadPhase::PostProcessing {
                    stage: "Extracting audio".to_string(),
                },
            }
        );
    }

    #[tokio::test]
    async fn streaming_progress_events_emit_without_treating_templates_as_filepaths() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        fs::create_dir_all(&dest).unwrap();
        let template_wav = write_wav_template(&ws.root.join("template.wav"));
        write_noop_executable(&ws.bin_dir, "ffmpeg");

        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                r#"printf 'TERMUSIC_PROG\tdownloading\t500\t1000\tNA\t250\t2\tStreamed Song\n'
printf 'TERMUSIC_POST\tstarted\tExtractAudio\tStreamed Song\n'
out="$stage/Streamed Song.wav"
cp "{template}" "$out"
printf '%s\n' "$out""#,
                template = template_wav.display(),
            ),
            &format!(
                "WScript.StdOut.WriteLine(\"TERMUSIC_PROG\\tdownloading\\t500\\t1000\\tNA\\t250\\t2\\tStreamed Song\");\n\
                 WScript.StdOut.WriteLine(\"TERMUSIC_POST\\tstarted\\tExtractAudio\\tStreamed Song\");\n\
                 var out = stage + \"\\\\Streamed Song.wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                template = js_escape(&template_wav),
            ),
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut opts = base_options(vec!["https://example.com/stream"]);
        opts.dest = Some(dest);
        opts.format = DownloadFormat::Wav;

        let outcome = run_download_with_env_and_progress(&ws.paths, &opts, &ws.env(None), Some(tx))
            .await
            .expect("streaming mock should succeed");
        assert_eq!(outcome.downloaded_files.len(), 1);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events.iter().any(|e| matches!(
            e,
            DownloadProgressEvent::BatchStarted { jobs: 4, .. }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            DownloadProgressEvent::ItemUpdate {
                index: 0,
                phase: DownloadPhase::Downloading {
                    downloaded_bytes: Some(500),
                    total_bytes: Some(1000),
                    ..
                },
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            DownloadProgressEvent::ItemUpdate {
                index: 0,
                phase: DownloadPhase::PostProcessing { stage },
                ..
            } if stage == "Extracting audio"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            DownloadProgressEvent::ItemUpdate {
                index: 0,
                phase: DownloadPhase::Completed { .. },
                ..
            }
        )));
        assert!(matches!(events.last(), Some(DownloadProgressEvent::BatchFinished)));
    }

    #[test]
    fn move_staged_file_returns_existing_destination_file_and_classifies_errors() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("dest");
        let canonical_dest = prepare_directory(&dest).unwrap();
        let staged_in_dest = canonical_dest.join("AlreadyThere.wav");
        fs::write(&staged_in_dest, b"audio").unwrap();

        let moved = move_staged_file(&staged_in_dest, &canonical_dest).unwrap();
        assert_eq!(moved, staged_in_dest);

        assert!(is_hardlink_unsupported(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        #[cfg(unix)]
        assert!(is_cross_device_error(&io::Error::from_raw_os_error(libc::EXDEV)));
        #[cfg(windows)]
        assert!(is_cross_device_error(&io::Error::from_raw_os_error(17)));
    }

    #[test]
    fn windows_cmd_batch_quoting_preserves_spaces_tabs_and_outer_quotes() {
        let bin = Path::new(r"C:\Program Files\yt-dlp\yt-dlp.cmd");
        let args = vec![
            OsString::from("--progress-template"),
            OsString::from(PROGRESS_TEMPLATE_DOWNLOAD),
            OsString::from("-P"),
            OsString::from(r"C:\Users\Alice Smith\Music\"),
            OsString::from("--newline"),
            OsString::from("https://example.com/?a=1&b=2"),
        ];
        let cmdline = format_windows_cmd_batch_line(bin, &args);
        let rendered = cmdline.to_string_lossy();
        assert!(rendered.starts_with('"') && rendered.ends_with('"'));
        assert!(rendered.contains(r#""C:\Program Files\yt-dlp\yt-dlp.cmd""#));
        assert!(rendered.contains(&format!("\"{PROGRESS_TEMPLATE_DOWNLOAD}\"")));
        assert!(rendered.contains(r#""C:\Users\Alice Smith\Music\\""#));
        assert!(rendered.contains(" --newline "));
        assert!(rendered.contains(r#""https://example.com/?a=1&b=2""#));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cmd_shim_in_spaced_dir_receives_tabbed_progress_template() {
        let ws = TestWorkspace::new();
        let spaced_bin = ws.root.join("dir with spaces");
        let spaced_stage = ws.root.join("stage with spaces");
        fs::create_dir_all(&spaced_bin).unwrap();
        fs::create_dir_all(&spaced_stage).unwrap();

        let template_wav = write_wav_template(&ws.root.join("template.wav"));
        let args_log = ws.root.join("spaced_args.log");
        write_mock_yt_dlp(
            &spaced_bin,
            "",
            &format!(
                "var ts = fso.OpenTextFile(\"{args_log}\", 2, true);\n\
                 for (var i = 0; i < allArgs.length; i++) ts.WriteLine(allArgs[i]);\n\
                 ts.Close();\n\
                 var out = stage + \"\\\\Spaced Track.wav\";\n\
                 fso.CopyFile(\"{template}\", out, true);\n\
                 WScript.StdOut.WriteLine(out);",
                args_log = js_escape(&args_log),
                template = js_escape(&template_wav),
            ),
        );

        let args = build_yt_dlp_args(
            DownloadFormat::Wav,
            false,
            &spaced_stage,
            None,
            None,
            false,
            "https://example.com/track",
        );
        let shim = spaced_bin.join("yt-dlp.cmd");
        let outputs = execute_yt_dlp(&shim, &args, &spaced_stage, &ws.env(None), 0, None)
            .await
            .expect("spaced .cmd shim succeeds");
        assert_eq!(outputs.len(), 1);
        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(logged.lines().any(|l| l == PROGRESS_TEMPLATE_DOWNLOAD));
        assert!(logged.lines().any(|l| l == spaced_stage.to_string_lossy()));
    }

    #[test]
    fn parse_download_format_str_accepts_optional_dot_and_mp4() {
        assert_eq!(parse_download_format_str("mp3").unwrap(), DownloadFormat::Mp3);
        assert_eq!(parse_download_format_str(".mp3").unwrap(), DownloadFormat::Mp3);
        assert_eq!(parse_download_format_str(" .MP4 ").unwrap(), DownloadFormat::Mp4);
        assert_eq!(parse_download_format_str("mp4").unwrap(), DownloadFormat::Mp4);
        assert_eq!(parse_download_format_str(".flac").unwrap(), DownloadFormat::Flac);
        assert_eq!(parse_download_format_str(".M4A").unwrap(), DownloadFormat::M4a);
        assert_eq!(parse_download_format_str("ogg").unwrap(), DownloadFormat::Ogg);
        assert_eq!(parse_download_format_str(".wav").unwrap(), DownloadFormat::Wav);
        assert_eq!(parse_download_format_str("aac").unwrap(), DownloadFormat::Aac);

        assert!(DownloadFormat::Mp3.is_audio());
        assert!(DownloadFormat::Flac.is_audio());
        assert!(!DownloadFormat::Mp4.is_audio());

        for bad in ["", "   ", ".", ".mkv", "avi"] {
            assert!(
                matches!(parse_download_format_str(bad), Err(TermusicError::InvalidArg(_))),
                "expected InvalidArg for {bad:?}"
            );
        }
    }

    #[test]
    fn build_yt_dlp_args_uses_video_preset_for_mp4() {
        let stage = PathBuf::from("/tmp/stage");
        let args = build_yt_dlp_args(
            DownloadFormat::Mp4,
            false,
            &stage,
            None,
            None,
            true,
            "https://example.com/video",
        );
        let str_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(str_args.windows(2).any(|w| w == ["-t", "mp4"]));
        assert!(str_args.contains(&"--add-metadata".to_string()));
        assert!(str_args.contains(&"--embed-thumbnail".to_string()));
        assert!(!str_args.contains(&"-x".to_string()));
        assert!(!str_args.contains(&"--audio-format".to_string()));
        assert!(!str_args.contains(&"--audio-quality".to_string()));
    }

    #[tokio::test]
    async fn mock_yt_dlp_downloads_mp4_and_skips_library_indexing_when_auto_add_enabled() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("videos");
        fs::create_dir_all(&dest).unwrap();
        write_noop_executable(&ws.bin_dir, "ffmpeg");

        let args_log = ws.root.join("mp4-args.txt");
        write_mock_yt_dlp(
            &ws.bin_dir,
            &format!(
                "printf '%s\\n' \"$@\" > \"{args_log}\"\nout=\"$stage/Video Clip.mp4\"\nprintf 'fake-mp4' > \"$out\"\nprintf '%s\\n' \"$out\"",
                args_log = args_log.display(),
            ),
            &format!(
                "var ts = fso.OpenTextFile(\"{args_log}\", 2, true);\n\
                 for (var i = 0; i < allArgs.length; i++) ts.WriteLine(allArgs[i]);\n\
                 ts.Close();\n\
                 var out = stage + \"\\\\Video Clip.mp4\";\n\
                 var outFile = fso.OpenTextFile(out, 2, true);\n\
                 outFile.Write(\"fake-mp4\");\n\
                 outFile.Close();\n\
                 WScript.StdOut.WriteLine(out);",
                args_log = js_escape(&args_log),
            ),
        );

        let mut opts = base_options(vec!["https://example.com/video"]);
        opts.dest = Some(dest.clone());
        opts.format = DownloadFormat::Mp4;
        opts.auto_add = true;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = run_download_with_env_and_progress(&ws.paths, &opts, &ws.env(None), Some(tx))
            .await
            .expect("mp4 download should succeed without failing library indexing");

        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.downloaded_files, vec![dest.join("Video Clip.mp4")]);
        assert!(outcome.add_summary.is_none());

        let lib = Library::load(&ws.paths.library_path).unwrap();
        assert!(lib.songs.is_empty());

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events.iter().any(|ev| matches!(
            ev,
            DownloadProgressEvent::Notice(msg) if msg.contains("mp4")
        )));

        let logged = fs::read_to_string(&args_log).unwrap();
        let logged_lines: Vec<&str> = logged.lines().collect();
        assert!(logged_lines.windows(2).any(|w| w == ["-t", "mp4"]));
        assert!(!logged_lines.contains(&"-x"));
    }
}
