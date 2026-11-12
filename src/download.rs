use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Result, TermusicError};
use crate::library::{AddSummary, Library};
use crate::paths::{AppPaths, load_workspace_config, set_workspace_download_dest};

const WSL_NODE_PATH: &str = "/mnt/c/Users/patri/scoop/apps/nodejs/current/node.exe";

/// Audio output container/codec format passed to `yt-dlp --audio-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum DownloadFormat {
    #[default]
    Mp3,
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
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Ogg => "ogg",
            Self::Wav => "wav",
            Self::Aac => "aac",
        }
    }

    /// Format argument expected by `yt-dlp --audio-format` (`ogg` -> `vorbis`).
    pub fn yt_dlp_format(self) -> &'static str {
        match self {
            Self::Ogg => "vorbis",
            other => other.as_str(),
        }
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
}

/// Result returned by [`run_download`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DownloadOutcome {
    pub saved_dest: Option<PathBuf>,
    pub downloaded_files: Vec<PathBuf>,
    pub add_summary: Option<AddSummary>,
}

/// Environment snapshot injected into [`run_download_with_env`] so unit tests
/// remain isolated without mutating process-wide environment variables.
#[derive(Debug, Clone)]
struct DownloadEnv {
    home_dir: Option<PathBuf>,
    path_var: Option<OsString>,
    termusic_dest: Option<String>,
    wsl_node_path: PathBuf,
}

impl DownloadEnv {
    fn from_system() -> Self {
        Self {
            home_dir: std::env::var_os("HOME").map(PathBuf::from),
            path_var: std::env::var_os("PATH"),
            termusic_dest: std::env::var("TERMUSIC_DEST").ok(),
            wsl_node_path: PathBuf::from(WSL_NODE_PATH),
        }
    }
}

/// Download tracks with `yt-dlp` and optionally index them into the workspace library.
pub fn run_download(paths: &AppPaths, opts: &DownloadOptions) -> Result<DownloadOutcome> {
    let env = DownloadEnv::from_system();
    run_download_with_env(paths, opts, &env)
}

fn run_download_with_env(
    paths: &AppPaths,
    opts: &DownloadOptions,
    env: &DownloadEnv,
) -> Result<DownloadOutcome> {
    let saved_dest = match opts.set_dest.as_deref() {
        Some(dir) => {
            validate_non_empty_path(dir, "--set-dest")?;
            Some(set_workspace_download_dest(paths, dir)?)
        }
        None => None,
    };

    if opts.urls.is_empty() {
        if saved_dest.is_some() {
            return Ok(DownloadOutcome {
                saved_dest,
                downloaded_files: Vec::new(),
                add_summary: None,
            });
        }
        return Err(TermusicError::InvalidArg(
            "at least one download URL is required".into(),
        ));
    }

    for url in &opts.urls {
        if url.trim().is_empty() {
            return Err(TermusicError::InvalidArg(
                "download URL cannot be empty".into(),
            ));
        }
    }

    let dest_dir = resolve_destination(paths, opts, saved_dest.as_deref(), env)?;
    let yt_dlp = find_yt_dlp(env)?;
    let _ffmpeg = find_ffmpeg(env)?;
    let js_runtime = detect_js_runtime(env);
    let embed_thumbnail = should_embed_thumbnail(opts, env);

    let args = build_yt_dlp_args(opts, &dest_dir, js_runtime.as_deref(), embed_thumbnail);
    let downloaded_files = execute_yt_dlp(&yt_dlp, &args, &dest_dir, env)?;

    let add_summary = if opts.auto_add && !downloaded_files.is_empty() {
        Some(index_downloaded_files(
            paths,
            &downloaded_files,
            opts.playlist_name.as_deref(),
        )?)
    } else {
        None
    };

    Ok(DownloadOutcome {
        saved_dest,
        downloaded_files,
        add_summary,
    })
}

fn validate_non_empty_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().to_string_lossy().trim().is_empty() {
        return Err(TermusicError::InvalidArg(format!(
            "{label} path cannot be empty"
        )));
    }
    Ok(())
}

fn resolve_destination(
    paths: &AppPaths,
    opts: &DownloadOptions,
    saved_dest: Option<&Path>,
    env: &DownloadEnv,
) -> Result<PathBuf> {
    let raw_dest: PathBuf = if let Some(dest) = opts.dest.as_deref() {
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
    Ok(fs::canonicalize(dir)?)
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && (meta.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

fn find_on_path(binary: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let paths = std::env::split_paths(path_var?);
    for dir in paths {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn find_yt_dlp(env: &DownloadEnv) -> Result<PathBuf> {
    if let Some(home) = env.home_dir.as_deref() {
        let local_bin = home.join(".local/bin/yt-dlp");
        if is_executable_file(&local_bin) {
            return Ok(local_bin);
        }
    }

    find_on_path("yt-dlp", env.path_var.as_deref()).ok_or_else(|| {
        TermusicError::NotFound("yt-dlp not found in ~/.local/bin or PATH".into())
    })
}

fn find_ffmpeg(env: &DownloadEnv) -> Result<PathBuf> {
    find_on_path("ffmpeg", env.path_var.as_deref())
        .or_else(|| find_on_path("avconv", env.path_var.as_deref()))
        .ok_or_else(|| {
            TermusicError::NotFound("neither ffmpeg nor avconv found in PATH".into())
        })
}

fn detect_js_runtime(env: &DownloadEnv) -> Option<String> {
    if find_on_path("deno", env.path_var.as_deref()).is_some() {
        Some("deno".into())
    } else if find_on_path("node", env.path_var.as_deref()).is_some() {
        Some("node".into())
    } else if is_executable_file(&env.wsl_node_path) {
        Some(format!("node:{}", env.wsl_node_path.display()))
    } else {
        None
    }
}

fn has_python_mutagen(env: &DownloadEnv) -> bool {
    let Some(python) = find_on_path("python3", env.path_var.as_deref()) else {
        return false;
    };

    let mut cmd = Command::new(python);
    cmd.args(["-c", "import mutagen"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path_var) = env.path_var.as_deref() {
        cmd.env("PATH", path_var);
    }

    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn should_embed_thumbnail(opts: &DownloadOptions, env: &DownloadEnv) -> bool {
    if !opts.embed_thumbnail {
        return false;
    }

    match opts.format {
        DownloadFormat::Wav | DownloadFormat::Aac => {
            eprintln!(
                "note: skipping thumbnail embedding for {} format",
                opts.format
            );
            false
        }
        DownloadFormat::Ogg => {
            if has_python_mutagen(env) {
                true
            } else {
                eprintln!(
                    "note: skipping thumbnail embedding for ogg (python3 mutagen not available)"
                );
                false
            }
        }
        DownloadFormat::Mp3 | DownloadFormat::Flac | DownloadFormat::M4a => true,
    }
}

fn build_yt_dlp_args(
    opts: &DownloadOptions,
    dest_dir: &Path,
    js_runtime: Option<&str>,
    embed_thumbnail: bool,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-f"),
        OsString::from("bestaudio/best"),
        OsString::from("-x"),
        OsString::from("--audio-format"),
        OsString::from(opts.format.yt_dlp_format()),
        OsString::from("--audio-quality"),
        OsString::from("0"),
        OsString::from("--add-metadata"),
    ];

    if embed_thumbnail {
        args.push(OsString::from("--embed-thumbnail"));
    }

    args.push(OsString::from(if opts.playlist {
        "--yes-playlist"
    } else {
        "--no-playlist"
    }));

    if let Some(runtime) = js_runtime {
        args.push(OsString::from("--js-runtimes"));
        args.push(OsString::from(runtime));
    }

    args.push(OsString::from("-P"));
    args.push(dest_dir.as_os_str().to_os_string());
    args.push(OsString::from("-o"));
    args.push(OsString::from("%(title)s.%(ext)s"));
    args.push(OsString::from("--print"));
    args.push(OsString::from("after_move:filepath"));

    for url in &opts.urls {
        args.push(OsString::from(url));
    }

    args
}

fn execute_yt_dlp(
    yt_dlp_bin: &Path,
    args: &[OsString],
    dest_dir: &Path,
    env: &DownloadEnv,
) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new(yt_dlp_bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path_var) = env.path_var.as_deref() {
        cmd.env("PATH", path_var);
    }
    if let Some(home) = env.home_dir.as_deref() {
        cmd.env("HOME", home);
    }

    let child = cmd.spawn()?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let code_info = output
            .status
            .code()
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "terminated by signal".into());
        return Err(TermusicError::Audio(format!(
            "yt-dlp failed ({code_info})"
        )));
    }

    Ok(parse_printed_filepaths(&output.stdout, dest_dir))
}

fn parse_printed_filepaths(stdout: &[u8], dest_dir: &Path) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    let mut seen = HashSet::new();
    let mut files = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let raw_path = PathBuf::from(trimmed);
        let resolved = if raw_path.is_relative() {
            dest_dir.join(raw_path)
        } else {
            raw_path
        };
        if seen.insert(resolved.clone()) {
            files.push(resolved);
        }
    }

    files
}

fn index_downloaded_files(
    paths: &AppPaths,
    files: &[PathBuf],
    playlist_name: Option<&str>,
) -> Result<AddSummary> {
    if let Some(parent) = paths.library_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut library = Library::load(&paths.library_path)?;
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

    library.save(&paths.library_path)?;
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::initialize_workspace_with_env;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "termusic-download-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            let canonical = fs::canonicalize(&dir).expect("canonicalize temp dir");
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
                1000,
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
                wsl_node_path: self.root.join("missing-wsl-node.exe"),
            }
        }
    }

    fn write_executable(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create script parent");
        }
        fs::write(path, content).expect("write script");
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod");
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
        }
    }

    #[test]
    fn missing_urls_without_set_dest_fails() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let opts = base_options(vec![]);

        let err = run_download_with_env(&ws.paths, &opts, &env).unwrap_err();
        assert!(matches!(err, TermusicError::InvalidArg(_)));
    }

    #[test]
    fn empty_dest_or_set_dest_is_rejected() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);

        let mut opts = base_options(vec!["https://example.com/a"]);
        opts.dest = Some(PathBuf::from("   "));
        assert!(matches!(
            run_download_with_env(&ws.paths, &opts, &env).unwrap_err(),
            TermusicError::InvalidArg(_)
        ));

        let mut set_only = base_options(vec![]);
        set_only.set_dest = Some(PathBuf::from(""));
        assert!(matches!(
            run_download_with_env(&ws.paths, &set_only, &env).unwrap_err(),
            TermusicError::InvalidArg(_)
        ));
    }

    #[test]
    fn set_dest_without_urls_persists_canonical_destination() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let target = ws.root.join("my_downloads");

        let mut opts = base_options(vec![]);
        opts.set_dest = Some(target.clone());

        let outcome = run_download_with_env(&ws.paths, &opts, &env).expect("set_dest");
        let canonical = fs::canonicalize(&target).expect("canonical");
        assert_eq!(outcome.saved_dest.as_ref(), Some(&canonical));
        assert!(outcome.downloaded_files.is_empty());
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
            fs::canonicalize(&config_dir).unwrap()
        );

        // 3rd: TERMUSIC_DEST overrides configured download_dest
        assert_eq!(
            resolve_destination(&ws.paths, &opts, None, &env_with_var).unwrap(),
            fs::canonicalize(&env_dir).unwrap()
        );

        // 2nd: saved_dest (--set-dest) overrides TERMUSIC_DEST
        fs::create_dir_all(&set_dir).unwrap();
        let canonical_set = fs::canonicalize(&set_dir).unwrap();
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
            fs::canonicalize(&flag_dir).unwrap()
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
        let path_yt = ws.bin_dir.join("yt-dlp");
        write_executable(&path_yt, "#!/bin/sh\nexit 0\n");
        assert_eq!(find_yt_dlp(&env).unwrap(), path_yt);

        let local_yt = ws.home_dir.join(".local/bin/yt-dlp");
        write_executable(&local_yt, "#!/bin/sh\nexit 0\n");
        assert_eq!(find_yt_dlp(&env).unwrap(), local_yt);

        // avconv fallback when ffmpeg absent, ffmpeg preferred when present
        let avconv = ws.bin_dir.join("avconv");
        write_executable(&avconv, "#!/bin/sh\nexit 0\n");
        assert_eq!(find_ffmpeg(&env).unwrap(), avconv);

        let ffmpeg = ws.bin_dir.join("ffmpeg");
        write_executable(&ffmpeg, "#!/bin/sh\nexit 0\n");
        assert_eq!(find_ffmpeg(&env).unwrap(), ffmpeg);

        // JS runtime priority: WSL node < node < deno
        let mut env_wsl = env.clone();
        env_wsl.wsl_node_path = ws.root.join("scoop-node.exe");
        write_executable(&env_wsl.wsl_node_path, "#!/bin/sh\nexit 0\n");
        assert_eq!(
            detect_js_runtime(&env_wsl).as_deref(),
            Some(format!("node:{}", env_wsl.wsl_node_path.display()).as_str())
        );

        let node = ws.bin_dir.join("node");
        write_executable(&node, "#!/bin/sh\nexit 0\n");
        assert_eq!(detect_js_runtime(&env_wsl).as_deref(), Some("node"));

        let deno = ws.bin_dir.join("deno");
        write_executable(&deno, "#!/bin/sh\nexit 0\n");
        assert_eq!(detect_js_runtime(&env_wsl).as_deref(), Some("deno"));
    }

    #[test]
    fn argument_mapping_and_thumbnail_guards() {
        let ws = TestWorkspace::new();
        let env = ws.env(None);
        let dest = ws.root.join("out");
        fs::create_dir_all(&dest).unwrap();

        // wav and aac never embed thumbnails; ogg checks python3 mutagen
        let mut wav_opts = base_options(vec!["https://example.com/1"]);
        wav_opts.format = DownloadFormat::Wav;
        assert!(!should_embed_thumbnail(&wav_opts, &env));

        let mut aac_opts = wav_opts.clone();
        aac_opts.format = DownloadFormat::Aac;
        assert!(!should_embed_thumbnail(&aac_opts, &env));

        let mut ogg_opts = wav_opts.clone();
        ogg_opts.format = DownloadFormat::Ogg;
        assert!(!should_embed_thumbnail(&ogg_opts, &env));

        // Mock python3 returning success for `import mutagen`
        let fake_py = ws.bin_dir.join("python3");
        write_executable(&fake_py, "#!/bin/sh\nexit 0\n");
        assert!(should_embed_thumbnail(&ogg_opts, &env));

        let args = build_yt_dlp_args(&ogg_opts, &dest, Some("node"), false);
        let str_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(str_args.windows(2).any(|w| w == ["--audio-format", "vorbis"]));
        assert!(str_args.contains(&"--no-playlist".to_string()));
        assert!(!str_args.contains(&"--embed-thumbnail".to_string()));
        assert!(str_args.windows(2).any(|w| w == ["--js-runtimes", "node"]));
        assert!(
            str_args
                .windows(2)
                .any(|w| w == ["--print", "after_move:filepath"])
        );

        let mut pl_opts = base_options(vec!["https://example.com/list"]);
        pl_opts.playlist = true;
        let pl_args: Vec<String> = build_yt_dlp_args(&pl_opts, &dest, None, true)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(pl_args.contains(&"--yes-playlist".to_string()));
        assert!(pl_args.contains(&"--embed-thumbnail".to_string()));
        assert!(!pl_args.contains(&"--js-runtimes".to_string()));
    }

    #[test]
    fn mock_yt_dlp_downloads_wav_and_respects_auto_add_and_playlist() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        fs::create_dir_all(&dest).unwrap();
        let track_path = dest.join("Synthetic Track.wav");
        write_synthetic_wav(&track_path, 0.25);

        write_executable(&ws.bin_dir.join("ffmpeg"), "#!/bin/sh\nexit 0\n");

        let args_log = ws.root.join("yt-dlp-args.txt");
        let fake_yt_dlp = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf '%s\\n' \"{}\"\n",
            args_log.display(),
            track_path.display()
        );
        write_executable(&ws.bin_dir.join("yt-dlp"), &fake_yt_dlp);

        let env = ws.env(None);

        // First run with auto_add = false does not touch library.json
        let mut no_add_opts = base_options(vec!["https://example.com/track"]);
        no_add_opts.dest = Some(dest.clone());
        no_add_opts.format = DownloadFormat::Wav;
        no_add_opts.auto_add = false;

        let no_add_out = run_download_with_env(&ws.paths, &no_add_opts, &env).expect("no-add run");
        assert_eq!(no_add_out.downloaded_files, vec![track_path.clone()]);
        assert!(no_add_out.add_summary.is_none());
        let lib_before = Library::load(&ws.paths.library_path).expect("load empty lib");
        assert!(lib_before.songs.is_empty());

        let logged_args = fs::read_to_string(&args_log).expect("read logged args");
        assert!(logged_args.lines().any(|l| l == "wav"));
        assert!(!logged_args.lines().any(|l| l == "--embed-thumbnail"));
        assert!(logged_args.lines().any(|l| l == "--no-playlist"));

        // Second run with auto_add = true and playlist_name creates song + playlist
        let mut add_opts = base_options(vec!["https://example.com/track"]);
        add_opts.dest = Some(dest.clone());
        add_opts.format = DownloadFormat::Wav;
        add_opts.playlist = true;
        add_opts.playlist_name = Some("Downloaded".into());

        let add_out = run_download_with_env(&ws.paths, &add_opts, &env).expect("auto-add run");
        let summary = add_out.add_summary.expect("add summary");
        assert_eq!(summary.added_count, 1);
        assert_eq!(summary.playlist_name.as_deref(), Some("Downloaded"));
        assert_eq!(summary.playlist_added_count, 1);

        let lib_after = Library::load(&ws.paths.library_path).expect("load indexed lib");
        assert_eq!(lib_after.songs.len(), 1);
        assert_eq!(lib_after.songs[0].path, track_path);
        assert_eq!(lib_after.playlists.len(), 1);
        assert_eq!(lib_after.playlists[0].name, "Downloaded");
        assert_eq!(lib_after.playlists[0].song_ids.len(), 1);

        let logged_args_2 = fs::read_to_string(&args_log).expect("read logged args 2");
        assert!(logged_args_2.lines().any(|l| l == "--yes-playlist"));
    }

    #[test]
    fn yt_dlp_non_zero_exit_returns_error() {
        let ws = TestWorkspace::new();
        let dest = ws.root.join("downloads");
        write_executable(&ws.bin_dir.join("ffmpeg"), "#!/bin/sh\nexit 0\n");
        write_executable(&ws.bin_dir.join("yt-dlp"), "#!/bin/sh\nexit 2\n");

        let env = ws.env(None);
        let mut opts = base_options(vec!["https://example.com/fail"]);
        opts.dest = Some(dest);

        let err = run_download_with_env(&ws.paths, &opts, &env).unwrap_err();
        assert!(matches!(err, TermusicError::Audio(_)));
    }
}
