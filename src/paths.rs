use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::download::DownloadFormat;
use crate::error::{Result, TermusicError};
pub use crate::platform::canonicalize_clean as normalize_canonical_path;
use crate::search::SearchProvider;

pub const WORKSPACE_CONFIG_VERSION: u32 = 1;
pub const DEFAULT_SEARCH_LIMIT: usize = 10;
pub const MAX_SEARCH_LIMIT: usize = 100;

const NOT_INITIALIZED_MSG: &str =
    "Termusic is not initialized. Run `termusic init <working-directory-location>` first.";

#[cfg(target_os = "macos")]
const UNIX_SOCKET_MAX_PATH_BYTES: usize = 104;
#[cfg(all(unix, not(target_os = "macos")))]
const UNIX_SOCKET_MAX_PATH_BYTES: usize = 108;

/// Cross-platform daemon IPC endpoint address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    #[cfg(unix)]
    UnixSocket(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
}

impl IpcEndpoint {
    /// Parse a CLI or daemon `--socket-path` override string into a platform endpoint.
    pub fn from_override(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TermusicError::InvalidArg(
                "--socket-path requires a non-empty endpoint".to_string(),
            ));
        }

        #[cfg(unix)]
        {
            Ok(Self::UnixSocket(PathBuf::from(raw)))
        }

        #[cfg(windows)]
        {
            if trimmed
                .get(..9)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\.\pipe\"))
            {
                if trimmed.len() == 9 {
                    return Err(TermusicError::InvalidArg(
                        "--socket-path named pipe name must not be empty".to_string(),
                    ));
                }
                Ok(Self::NamedPipe(trimmed.to_string()))
            } else {
                Ok(Self::NamedPipe(named_pipe_for_seed(trimmed)))
            }
        }
    }

    /// Borrow the underlying Unix socket path if this endpoint uses Unix domain sockets.
    #[cfg(unix)]
    pub fn as_unix_socket(&self) -> Option<&Path> {
        match self {
            Self::UnixSocket(path) => Some(path),
        }
    }

    /// Format this endpoint as a CLI argument or display token.
    pub fn to_arg_string(&self) -> String {
        match self {
            #[cfg(unix)]
            Self::UnixSocket(path) => path.to_string_lossy().into_owned(),
            #[cfg(windows)]
            Self::NamedPipe(name) => name.clone(),
        }
    }
}

impl fmt::Display for IpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            Self::UnixSocket(path) => write!(f, "{}", path.display()),
            #[cfg(windows)]
            Self::NamedPipe(name) => f.write_str(name),
        }
    }
}

/// Workspace metadata persisted at `<working-dir>/config/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub version: u32,
    pub working_dir: PathBuf,
    pub config_dir: PathBuf,
    pub playlists_dir: PathBuf,
    pub music_dir: PathBuf,
    #[serde(default)]
    pub download_dest: Option<PathBuf>,
    #[serde(default)]
    pub search_limit: Option<usize>,
    #[serde(default)]
    pub search_provider: Option<SearchProvider>,
    #[serde(default)]
    pub search_format: Option<DownloadFormat>,
}

/// Bootstrap pointer persisted at `$XDG_CONFIG_HOME/termusic/workspace.json` (or OS config dir).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePointer {
    pub working_dir: PathBuf,
}

/// Resolved filesystem paths for Termusic workspace, library data, and daemon IPC runtime files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub working_dir: PathBuf,
    pub config_path: PathBuf,
    pub playlists_dir: PathBuf,
    pub music_dir: PathBuf,
    pub library_path: PathBuf,
    pub ipc_endpoint: IpcEndpoint,
    pub pid_path: PathBuf,
    pub log_path: PathBuf,
}

impl AppPaths {
    /// Construct paths rooted at explicit `data_dir` and `runtime_dir` (for isolated tests).
    pub fn from_roots(data_dir: impl AsRef<Path>, runtime_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref();
        let runtime_dir = runtime_dir.as_ref();
        #[cfg(unix)]
        let ipc_endpoint = IpcEndpoint::UnixSocket(runtime_dir.join("termusic.sock"));
        #[cfg(windows)]
        let ipc_endpoint =
            IpcEndpoint::NamedPipe(named_pipe_for_seed(&runtime_dir.to_string_lossy()));

        Self {
            working_dir: data_dir.to_path_buf(),
            config_path: data_dir.join("config").join("config.json"),
            playlists_dir: data_dir.join("playlists"),
            music_dir: data_dir.join("music"),
            library_path: data_dir.join("library.json"),
            ipc_endpoint,
            pid_path: runtime_dir.join("termusic.pid"),
            log_path: runtime_dir.join("termusic-daemon.log"),
        }
    }

    /// Resolve workspace and runtime paths for the current user environment.
    /// Fails with `TermusicError::NotInitialized` if `termusic init` has not been run.
    pub fn resolve() -> Result<Self> {
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let home_dir = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let os_config_dir = dirs::config_dir();
        let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
        let runtime_id = crate::platform::user_runtime_id();
        let runtime_env = RuntimeEnv::from_env(home_dir.clone());

        Self::resolve_from_env_full(
            xdg_config_home,
            home_dir,
            os_config_dir,
            xdg_runtime_dir,
            &runtime_id,
            &runtime_env,
        )
    }

    pub fn resolve_from_env(
        xdg_config_home: Option<OsString>,
        home_dir: Option<PathBuf>,
        xdg_runtime_dir: Option<OsString>,
        runtime_id: &str,
    ) -> Result<Self> {
        let runtime_env = RuntimeEnv::from_home(home_dir.clone());
        Self::resolve_from_env_full(
            xdg_config_home,
            home_dir,
            None,
            xdg_runtime_dir,
            runtime_id,
            &runtime_env,
        )
    }

    fn resolve_from_env_full(
        xdg_config_home: Option<OsString>,
        home_dir: Option<PathBuf>,
        os_config_dir: Option<PathBuf>,
        xdg_runtime_dir: Option<OsString>,
        runtime_id: &str,
        runtime_env: &RuntimeEnv,
    ) -> Result<Self> {
        let pointer_path =
            resolve_workspace_pointer_path_with_os(xdg_config_home, home_dir, os_config_dir)?;
        let (ipc_endpoint, pid_path, log_path) =
            resolve_runtime_paths_with_env(xdg_runtime_dir, runtime_id, runtime_env);
        let cfg = with_file_lock(&lock_path_for(&pointer_path), false, || {
            if !pointer_path.exists() {
                return Err(TermusicError::NotInitialized(
                    NOT_INITIALIZED_MSG.to_string(),
                ));
            }

            let pointer_raw = fs::read_to_string(&pointer_path)?;
            let pointer: WorkspacePointer = serde_json::from_str(&pointer_raw)?;
            let config_path = pointer.working_dir.join("config").join("config.json");
            if !config_path.exists() {
                return Err(TermusicError::NotInitialized(
                    NOT_INITIALIZED_MSG.to_string(),
                ));
            }

            with_file_lock(&lock_path_for(&config_path), false, || {
                load_workspace_config_unlocked(&config_path)
            })
        })?;

        Ok(Self {
            working_dir: cfg.working_dir,
            config_path: cfg.config_dir.join("config.json"),
            library_path: cfg.playlists_dir.join("library.json"),
            playlists_dir: cfg.playlists_dir,
            music_dir: cfg.music_dir,
            ipc_endpoint,
            pid_path,
            log_path,
        })
    }

    /// Ensure parent directories exist for library and runtime files.
    pub fn ensure_dirs(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &self.ipc_endpoint;
            if let Some(parent) = socket_path.parent()
                && !parent.as_os_str().is_empty()
            {
                if parent
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("termusic-"))
                {
                    crate::platform::ensure_private_dir(parent)?;
                } else {
                    fs::create_dir_all(parent)?;
                }
            }
        }

        for path in [&self.library_path, &self.pid_path, &self.log_path] {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    }

    /// Lock file guarding single-instance daemon ownership for `pid_path`.
    pub fn daemon_lock_path(&self) -> PathBuf {
        suffix_path(&self.pid_path, "daemon.lock")
    }

    /// Lock file serializing concurrent daemon auto-spawn attempts for `pid_path`.
    pub fn daemon_spawn_lock_path(&self) -> PathBuf {
        suffix_path(&self.pid_path, "spawn.lock")
    }
}

/// Append `.<suffix>` to the full filename of `target` without replacing existing extensions.
pub fn suffix_path(target: &Path, suffix: &str) -> PathBuf {
    let suffix = suffix.trim_start_matches('.');
    match target.file_name() {
        Some(name) => {
            let mut new_name = OsString::from(name);
            new_name.push(".");
            new_name.push(suffix);
            target.with_file_name(new_name)
        }
        None => {
            let mut raw = target.as_os_str().to_os_string();
            raw.push(".");
            raw.push(suffix);
            PathBuf::from(raw)
        }
    }
}

/// Derive the sidecar advisory lock file path (`<target>.lock`) for `target`.
pub fn lock_path_for(target: &Path) -> PathBuf {
    suffix_path(target, "lock")
}

/// Open or create `lock_path` and acquire an OS advisory lock without truncating or unlinking it.
pub fn acquire_file_lock(
    lock_path: &Path,
    exclusive: bool,
    blocking: bool,
) -> Result<Option<File>> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;

    let map_try = |res: std::result::Result<(), std::fs::TryLockError>| -> Result<bool> {
        match res {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(err)) => Err(err.into()),
        }
    };

    let acquired = match (exclusive, blocking) {
        (true, true) => {
            file.lock()?;
            true
        }
        (false, true) => {
            file.lock_shared()?;
            true
        }
        (true, false) => map_try(file.try_lock())?,
        (false, false) => map_try(file.try_lock_shared())?,
    };

    if acquired {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

/// Execute `f` while holding a blocking advisory file lock on `lock_path`.
pub fn with_file_lock<T>(
    lock_path: &Path,
    exclusive: bool,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _guard = acquire_file_lock(lock_path, exclusive, true)?.ok_or_else(|| {
        TermusicError::Io(io::Error::other(format!(
            "blocking file lock returned no handle for {}",
            lock_path.display()
        )))
    })?;
    f()
}

/// Initialize a Termusic working directory and persist the bootstrap workspace pointer.
pub fn initialize_workspace(dir: &Path) -> Result<WorkspaceConfig> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home_dir = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let os_config_dir = dirs::config_dir();
    let current_dir = std::env::current_dir()?;
    initialize_workspace_with_env_and_os(dir, &current_dir, xdg_config_home, home_dir, os_config_dir)
}

/// Environment-parameterized workspace initialization helper (for isolated unit tests).
pub fn initialize_workspace_with_env(
    dir: &Path,
    current_dir: &Path,
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
) -> Result<WorkspaceConfig> {
    initialize_workspace_with_env_and_os(dir, current_dir, xdg_config_home, home_dir, None)
}

fn initialize_workspace_with_env_and_os(
    dir: &Path,
    current_dir: &Path,
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
    os_config_dir: Option<PathBuf>,
) -> Result<WorkspaceConfig> {
    if dir.as_os_str().is_empty() || dir.to_str().is_some_and(|s| s.trim().is_empty()) {
        return Err(TermusicError::InvalidArg(
            "working directory path cannot be empty".to_string(),
        ));
    }

    let candidate = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        current_dir.join(dir)
    };

    if candidate.exists() && !candidate.is_dir() {
        return Err(TermusicError::InvalidArg(format!(
            "working directory path exists and is not a directory: {}",
            candidate.display()
        )));
    }

    fs::create_dir_all(&candidate)?;
    let working_dir = normalize_canonical_path(&candidate)?;

    let config_dir = working_dir.join("config");
    let playlists_dir = working_dir.join("playlists");
    let music_dir = working_dir.join("music");

    fs::create_dir_all(&config_dir)?;
    fs::create_dir_all(&playlists_dir)?;
    fs::create_dir_all(&music_dir)?;

    let workspace_cfg = WorkspaceConfig {
        version: WORKSPACE_CONFIG_VERSION,
        working_dir: working_dir.clone(),
        config_dir: config_dir.clone(),
        playlists_dir,
        music_dir,
        download_dest: None,
        search_limit: None,
        search_provider: None,
        search_format: None,
    };

    let config_file = config_dir.join("config.json");
    let pointer_path =
        resolve_workspace_pointer_path_with_os(xdg_config_home, home_dir, os_config_dir)?;
    with_file_lock(&lock_path_for(&pointer_path), true, || {
        with_file_lock(&lock_path_for(&config_file), true, || {
            write_json_atomic(&config_file, &workspace_cfg)
        })?;
        let pointer = WorkspacePointer { working_dir };
        write_json_atomic(&pointer_path, &pointer)
    })?;

    Ok(workspace_cfg)
}

fn load_workspace_config_unlocked(config_path: &Path) -> Result<WorkspaceConfig> {
    let raw = fs::read_to_string(config_path)?;
    let cfg: WorkspaceConfig = serde_json::from_str(&raw)?;
    Ok(cfg)
}

/// Load and deserialize `WorkspaceConfig` from `paths.config_path`.
pub fn load_workspace_config(paths: &AppPaths) -> Result<WorkspaceConfig> {
    with_file_lock(&lock_path_for(&paths.config_path), false, || {
        load_workspace_config_unlocked(&paths.config_path)
    })
}

/// Persist `download_dest` inside `paths.config_path` atomically after creating and canonicalizing `dir`.
pub fn set_workspace_download_dest(paths: &AppPaths, dir: &Path) -> Result<PathBuf> {
    let current_dir = std::env::current_dir()?;
    set_workspace_download_dest_with_cwd(paths, dir, &current_dir)
}

fn set_workspace_download_dest_with_cwd(
    paths: &AppPaths,
    dir: &Path,
    current_dir: &Path,
) -> Result<PathBuf> {
    if dir.as_os_str().is_empty() || dir.to_str().is_some_and(|s| s.trim().is_empty()) {
        return Err(TermusicError::InvalidArg(
            "--set-dest requires a non-empty directory path".to_string(),
        ));
    }

    let candidate = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        current_dir.join(dir)
    };

    if candidate.exists() && !candidate.is_dir() {
        return Err(TermusicError::InvalidArg(format!(
            "download destination exists and is not a directory: {}",
            candidate.display()
        )));
    }

    fs::create_dir_all(&candidate)?;
    let abs = normalize_canonical_path(&candidate)?;

    with_file_lock(&lock_path_for(&paths.config_path), true, || {
        let mut cfg = load_workspace_config_unlocked(&paths.config_path)?;
        cfg.download_dest = Some(abs.clone());
        write_json_atomic(&paths.config_path, &cfg)?;
        Ok(())
    })?;
    Ok(abs)
}

/// Atomically update `paths.config_path` under an exclusive advisory file lock.
fn update_workspace_config<T>(
    paths: &AppPaths,
    mutate: impl FnOnce(&mut WorkspaceConfig) -> T,
) -> Result<T> {
    with_file_lock(&lock_path_for(&paths.config_path), true, || {
        let mut cfg = load_workspace_config_unlocked(&paths.config_path)?;
        let out = mutate(&mut cfg);
        write_json_atomic(&paths.config_path, &cfg)?;
        Ok(out)
    })
}

/// Validate that a search result limit is within `1..=MAX_SEARCH_LIMIT`.
pub fn validate_search_limit(limit: usize) -> Result<usize> {
    if limit == 0 || limit > MAX_SEARCH_LIMIT {
        return Err(TermusicError::InvalidArg(format!(
            "search limit must be between 1 and {MAX_SEARCH_LIMIT}, got {limit}"
        )));
    }
    Ok(limit)
}

/// Persist `search_limit` inside `paths.config_path` atomically after validating `limit`.
pub fn set_workspace_search_limit(paths: &AppPaths, limit: usize) -> Result<usize> {
    let valid = validate_search_limit(limit)?;
    update_workspace_config(paths, |cfg| {
        cfg.search_limit = Some(valid);
        valid
    })
}

/// Persist `search_provider` inside `paths.config_path` atomically.
pub fn set_workspace_search_provider(
    paths: &AppPaths,
    provider: SearchProvider,
) -> Result<SearchProvider> {
    update_workspace_config(paths, |cfg| {
        cfg.search_provider = Some(provider);
        provider
    })
}

/// Persist `search_format` inside `paths.config_path` atomically.
pub fn set_workspace_search_format(
    paths: &AppPaths,
    format: DownloadFormat,
) -> Result<DownloadFormat> {
    update_workspace_config(paths, |cfg| {
        cfg.search_format = Some(format);
        format
    })
}

fn resolve_workspace_pointer_path_with_os(
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
    os_config_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    let config_root = match xdg_config_home.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                if let Some(dir) = os_config_dir.filter(|p| !p.as_os_str().is_empty()) {
                    dir
                } else if let Some(home) = home_dir.filter(|p| !p.as_os_str().is_empty()) {
                    home.join(".config")
                } else {
                    return Err(TermusicError::Io(io::Error::new(
                        io::ErrorKind::NotFound,
                        "could not determine XDG_CONFIG_HOME or home directory",
                    )));
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                match home_dir.filter(|p| !p.as_os_str().is_empty()) {
                    Some(home) => home.join(".config"),
                    None => match os_config_dir.filter(|p| !p.as_os_str().is_empty()) {
                        Some(dir) => dir,
                        None => {
                            return Err(TermusicError::Io(io::Error::new(
                                io::ErrorKind::NotFound,
                                "could not determine XDG_CONFIG_HOME or home directory",
                            )));
                        }
                    },
                }
            }
        }
    };
    Ok(config_root.join("termusic").join("workspace.json"))
}

#[derive(Debug, Clone, Default)]
struct RuntimeEnv {
    #[cfg(all(unix, not(target_os = "macos")))]
    is_termux: bool,
    #[cfg(all(unix, not(target_os = "macos")))]
    tmpdir: Option<OsString>,
    #[cfg(all(unix, not(target_os = "macos")))]
    prefix: Option<OsString>,
    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    home_dir: Option<PathBuf>,
    #[cfg(windows)]
    local_app_data: Option<PathBuf>,
}

impl RuntimeEnv {
    fn from_home(home_dir: Option<PathBuf>) -> Self {
        #[cfg(target_os = "macos")]
        {
            let _ = home_dir;
            Self {}
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                home_dir,
                ..Default::default()
            }
        }
    }

    fn from_env(home_dir: Option<PathBuf>) -> Self {
        #[cfg(target_os = "macos")]
        {
            let _ = home_dir;
            Self {}
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self {
                is_termux: crate::platform::is_termux(),
                tmpdir: std::env::var_os("TMPDIR"),
                prefix: std::env::var_os("PREFIX"),
                home_dir,
            }
        }
        #[cfg(windows)]
        {
            Self {
                home_dir,
                local_app_data: std::env::var_os("LOCALAPPDATA")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .or_else(dirs::data_local_dir),
            }
        }
    }
}

fn sanitize_runtime_id(runtime_id: &str) -> String {
    let clean: String = runtime_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "user".to_string()
    } else {
        clean
    }
}

#[cfg(unix)]
fn fits_unix_socket_limit(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().len() < UNIX_SOCKET_MAX_PATH_BYTES
}

#[cfg(windows)]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(windows)]
fn named_pipe_for_seed(seed: &str) -> String {
    format!(r"\\.\pipe\termusic-{:016x}", fnv1a_64(seed.as_bytes()))
}

fn resolve_runtime_paths_with_env(
    xdg_runtime_dir: Option<OsString>,
    runtime_id: &str,
    runtime_env: &RuntimeEnv,
) -> (IpcEndpoint, PathBuf, PathBuf) {
    let safe_id = sanitize_runtime_id(runtime_id);

    #[cfg(windows)]
    {
        let pipe_suffix = if safe_id.len() <= 128 {
            safe_id.clone()
        } else {
            format!("{:016x}", fnv1a_64(safe_id.as_bytes()))
        };
        let endpoint = IpcEndpoint::NamedPipe(format!(r"\\.\pipe\termusic-{pipe_suffix}"));

        let run_dir = xdg_runtime_dir
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                runtime_env
                    .local_app_data
                    .as_ref()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.join("termusic").join("run"))
            })
            .or_else(|| {
                runtime_env
                    .home_dir
                    .as_ref()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|h| h.join("AppData").join("Local").join("termusic").join("run"))
            })
            .unwrap_or_else(|| std::env::temp_dir().join(format!("termusic-{pipe_suffix}")));

        return (
            endpoint,
            run_dir.join("termusic.pid"),
            run_dir.join("termusic-daemon.log"),
        );
    }

    #[cfg(target_os = "macos")]
    {
        let _ = runtime_env;
        if let Some(runtime_dir) = xdg_runtime_dir.filter(|v| !v.is_empty()) {
            let dir = PathBuf::from(runtime_dir);
            let sock = dir.join("termusic.sock");
            if fits_unix_socket_limit(&sock) {
                return (
                    IpcEndpoint::UnixSocket(sock),
                    dir.join("termusic.pid"),
                    dir.join("termusic-daemon.log"),
                );
            }
        }

        let dir = PathBuf::from(format!("/tmp/termusic-{safe_id}"));
        let sock = dir.join("termusic.sock");
        if fits_unix_socket_limit(&sock) {
            return (
                IpcEndpoint::UnixSocket(sock),
                dir.join("termusic.pid"),
                dir.join("termusic-daemon.log"),
            );
        }

        let short = PathBuf::from("/tmp/termusic-user");
        return (
            IpcEndpoint::UnixSocket(short.join("termusic.sock")),
            short.join("termusic.pid"),
            short.join("termusic-daemon.log"),
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if runtime_env.is_termux || cfg!(target_os = "android") {
            let mut candidates: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();
            if let Some(dir) = xdg_runtime_dir.filter(|v| !v.is_empty()).map(PathBuf::from) {
                candidates.push((
                    dir.join("termusic.sock"),
                    dir.join("termusic.pid"),
                    dir.join("termusic-daemon.log"),
                ));
            }
            if let Some(dir) = runtime_env
                .tmpdir
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
            {
                candidates.push((
                    dir.join(format!("termusic-{safe_id}.sock")),
                    dir.join(format!("termusic-{safe_id}.pid")),
                    dir.join(format!("termusic-{safe_id}-daemon.log")),
                ));
            }
            if let Some(prefix) = runtime_env
                .prefix
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
            {
                let var_run = prefix.join("var").join("run");
                candidates.push((
                    var_run.join("termusic.sock"),
                    var_run.join("termusic.pid"),
                    var_run.join("termusic-daemon.log"),
                ));
                let prefix_tmp = prefix.join("tmp");
                candidates.push((
                    prefix_tmp.join(format!("termusic-{safe_id}.sock")),
                    prefix_tmp.join(format!("termusic-{safe_id}.pid")),
                    prefix_tmp.join(format!("termusic-{safe_id}-daemon.log")),
                ));
            }
            if let Some(home) = runtime_env
                .home_dir
                .as_ref()
                .filter(|p| !p.as_os_str().is_empty())
            {
                let cache_run = home.join(".cache").join("termusic").join("run");
                candidates.push((
                    cache_run.join("termusic.sock"),
                    cache_run.join("termusic.pid"),
                    cache_run.join("termusic-daemon.log"),
                ));
            }

            if let Some((sock, pid, log)) = candidates
                .into_iter()
                .find(|(sock, _, _)| fits_unix_socket_limit(sock))
            {
                return (IpcEndpoint::UnixSocket(sock), pid, log);
            }
        } else if let Some(runtime_dir) = xdg_runtime_dir.filter(|v| !v.is_empty()) {
            let dir = PathBuf::from(runtime_dir);
            return (
                IpcEndpoint::UnixSocket(dir.join("termusic.sock")),
                dir.join("termusic.pid"),
                dir.join("termusic-daemon.log"),
            );
        }

        let tmp = PathBuf::from("/tmp");
        (
            IpcEndpoint::UnixSocket(tmp.join(format!("termusic-{safe_id}.sock"))),
            tmp.join(format!("termusic-{safe_id}.pid")),
            tmp.join(format!("termusic-{safe_id}-daemon.log")),
        )
    }
}

/// Serialize `value` as pretty JSON and atomically replace `path` via a unique sibling temp file.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let pid = std::process::id();
    let unique = uuid::Uuid::new_v4();
    let tmp_path = match path.file_name() {
        Some(name) => {
            let mut tmp_name = OsString::from(name);
            tmp_name.push(format!(".{pid}.{unique}.tmp"));
            path.with_file_name(tmp_name)
        }
        None => {
            let mut raw = path.as_os_str().to_os_string();
            raw.push(format!(".{pid}.{unique}.tmp"));
            PathBuf::from(raw)
        }
    };

    let bytes = serde_json::to_vec_pretty(value)?;
    let write_result = (|| -> Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-paths-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn from_roots_places_expected_files() {
        let paths = AppPaths::from_roots("/tmp/data/termusic", "/tmp/run");
        assert_eq!(
            paths.working_dir,
            PathBuf::from("/tmp/data/termusic")
        );
        assert_eq!(
            paths.config_path,
            PathBuf::from("/tmp/data/termusic/config/config.json")
        );
        assert_eq!(
            paths.playlists_dir,
            PathBuf::from("/tmp/data/termusic/playlists")
        );
        assert_eq!(paths.music_dir, PathBuf::from("/tmp/data/termusic/music"));
        assert_eq!(
            paths.library_path,
            PathBuf::from("/tmp/data/termusic/library.json")
        );
        #[cfg(unix)]
        assert_eq!(
            paths.ipc_endpoint,
            IpcEndpoint::UnixSocket(PathBuf::from("/tmp/run/termusic.sock"))
        );
        #[cfg(windows)]
        assert!(matches!(
            paths.ipc_endpoint,
            IpcEndpoint::NamedPipe(ref pipe) if pipe.starts_with(r"\\.\pipe\termusic-")
        ));
        assert_eq!(paths.pid_path, PathBuf::from("/tmp/run/termusic.pid"));
        assert_eq!(
            paths.log_path,
            PathBuf::from("/tmp/run/termusic-daemon.log")
        );
    }

    #[test]
    fn resolve_fails_with_not_initialized_when_workspace_missing() {
        let root = temp_dir("uninit");
        let cfg_home = root.join("xdg_config");
        fs::create_dir_all(&cfg_home).unwrap();

        let err = AppPaths::resolve_from_env(
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
            Some(OsString::from("/run/user/1000")),
            "1000",
        )
        .unwrap_err();
        assert!(matches!(err, TermusicError::NotInitialized(msg) if msg.contains("termusic init")));

        // Pointer exists, but config/config.json does not exist -> also NotInitialized.
        let pointer_path = cfg_home.join("termusic").join("workspace.json");
        write_json_atomic(
            &pointer_path,
            &WorkspacePointer {
                working_dir: root.join("missing-ws"),
            },
        )
        .unwrap();

        let err2 = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(OsString::from("/run/user/1000")),
            "1000",
        )
        .unwrap_err();
        assert!(matches!(err2, TermusicError::NotInitialized(_)));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_and_resolve_with_xdg_env_vars() {
        let root = temp_dir("init-xdg");
        let cwd = root.join("cwd");
        let cfg_home = root.join("xdg_config");
        #[cfg(unix)]
        let run_dir = PathBuf::from("/tmp").join(format!(
            "tm-pxdg-{}",
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        #[cfg(windows)]
        let run_dir = root.join("run");
        fs::create_dir_all(&cwd).unwrap();

        let cfg = initialize_workspace_with_env(
            Path::new("my-ws"),
            &cwd,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();

        let canonical_ws = normalize_canonical_path(&cwd.join("my-ws")).unwrap();
        assert_eq!(cfg.version, WORKSPACE_CONFIG_VERSION);
        assert_eq!(cfg.working_dir, canonical_ws);
        assert_eq!(cfg.config_dir, canonical_ws.join("config"));
        assert_eq!(cfg.playlists_dir, canonical_ws.join("playlists"));
        assert_eq!(cfg.music_dir, canonical_ws.join("music"));
        assert!(cfg.config_dir.join("config.json").exists());
        assert!(cfg.playlists_dir.is_dir());
        assert!(cfg.music_dir.is_dir());
        assert!(cfg_home.join("termusic").join("workspace.json").exists());

        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.clone().into_os_string()),
            "1000",
        )
        .unwrap();

        assert_eq!(paths.working_dir, canonical_ws);
        assert_eq!(paths.config_path, canonical_ws.join("config/config.json"));
        assert_eq!(paths.playlists_dir, canonical_ws.join("playlists"));
        assert_eq!(paths.music_dir, canonical_ws.join("music"));
        assert_eq!(
            paths.library_path,
            canonical_ws.join("playlists/library.json")
        );
        #[cfg(unix)]
        assert_eq!(
            paths.ipc_endpoint,
            IpcEndpoint::UnixSocket(run_dir.join("termusic.sock"))
        );
        #[cfg(windows)]
        assert_eq!(
            paths.ipc_endpoint,
            IpcEndpoint::NamedPipe(r"\\.\pipe\termusic-1000".to_string())
        );
        assert_eq!(paths.pid_path, run_dir.join("termusic.pid"));
        assert_eq!(paths.log_path, run_dir.join("termusic-daemon.log"));

        let _ = fs::remove_dir_all(&run_dir);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_falls_back_when_xdg_vars_unset_or_empty() {
        let root = temp_dir("fallback");
        let ws = root.join("workspace");
        let home = root.join("home");

        initialize_workspace_with_env(
            &ws,
            &root,
            Some(OsString::from("")),
            Some(home.clone()),
        )
        .unwrap();

        let canonical_ws = normalize_canonical_path(&ws).unwrap();
        assert!(home.join(".config/termusic/workspace.json").exists());

        let paths = AppPaths::resolve_from_env(
            Some(OsString::from("")),
            Some(home.clone()),
            None,
            "1042",
        )
        .unwrap();

        assert_eq!(paths.working_dir, canonical_ws);
        assert_eq!(
            paths.library_path,
            canonical_ws.join("playlists/library.json")
        );
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
        {
            assert_eq!(
                paths.ipc_endpoint,
                IpcEndpoint::UnixSocket(PathBuf::from("/tmp/termusic-1042.sock"))
            );
            assert_eq!(paths.pid_path, PathBuf::from("/tmp/termusic-1042.pid"));
            assert_eq!(
                paths.log_path,
                PathBuf::from("/tmp/termusic-1042-daemon.log")
            );
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                paths.ipc_endpoint,
                IpcEndpoint::UnixSocket(PathBuf::from("/tmp/termusic-1042/termusic.sock"))
            );
            assert_eq!(
                paths.pid_path,
                PathBuf::from("/tmp/termusic-1042/termusic.pid")
            );
            assert_eq!(
                paths.log_path,
                PathBuf::from("/tmp/termusic-1042/termusic-daemon.log")
            );
        }
        #[cfg(target_os = "android")]
        {
            assert_eq!(
                paths.ipc_endpoint,
                IpcEndpoint::UnixSocket(home.join(".cache/termusic/run/termusic.sock"))
            );
            assert_eq!(
                paths.pid_path,
                home.join(".cache/termusic/run/termusic.pid")
            );
            assert_eq!(
                paths.log_path,
                home.join(".cache/termusic/run/termusic-daemon.log")
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                paths.ipc_endpoint,
                IpcEndpoint::NamedPipe(r"\\.\pipe\termusic-1042".to_string())
            );
            assert_eq!(
                paths.pid_path,
                home.join("AppData")
                    .join("Local")
                    .join("termusic")
                    .join("run")
                    .join("termusic.pid")
            );
            assert_eq!(
                paths.log_path,
                home.join("AppData")
                    .join("Local")
                    .join("termusic")
                    .join("run")
                    .join("termusic-daemon.log")
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn set_workspace_download_dest_persists_and_validates() {
        let root = temp_dir("dl-dest");
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        let init_cfg = initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();
        assert_eq!(init_cfg.download_dest, None);

        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();
        assert_eq!(load_workspace_config(&paths).unwrap().download_dest, None);

        // Empty/whitespace and regular-file destinations are rejected.
        assert!(matches!(
            set_workspace_download_dest_with_cwd(&paths, Path::new(""), &root),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            set_workspace_download_dest_with_cwd(&paths, Path::new("   "), &root),
            Err(TermusicError::InvalidArg(_))
        ));
        let not_a_dir = root.join("regular-file.txt");
        fs::write(&not_a_dir, b"hi").unwrap();
        assert!(matches!(
            set_workspace_download_dest_with_cwd(&paths, &not_a_dir, &root),
            Err(TermusicError::InvalidArg(_))
        ));

        // Relative destination resolves against cwd, creates dir, canonicalizes, and persists.
        let saved =
            set_workspace_download_dest_with_cwd(&paths, Path::new("downloads/sub"), &root)
                .unwrap();
        let expected = normalize_canonical_path(&root.join("downloads/sub")).unwrap();
        assert_eq!(saved, expected);
        assert!(expected.is_dir());

        let reloaded = load_workspace_config(&paths).unwrap();
        assert_eq!(reloaded.download_dest, Some(expected));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lock_and_suffix_helpers_support_shared_and_exclusive_modes() {
        let root = temp_dir("locks");
        let target = root.join("playlists").join("library.json");
        assert_eq!(
            lock_path_for(&target),
            root.join("playlists").join("library.json.lock")
        );

        let paths = AppPaths::from_roots(root.join("data"), root.join("run"));
        assert_eq!(
            paths.daemon_lock_path(),
            root.join("run").join("termusic.pid.daemon.lock")
        );
        assert_eq!(
            paths.daemon_spawn_lock_path(),
            root.join("run").join("termusic.pid.spawn.lock")
        );

        let lock_path = lock_path_for(&target);
        let exclusive = acquire_file_lock(&lock_path, true, false).unwrap();
        assert!(exclusive.is_some());
        assert!(acquire_file_lock(&lock_path, true, false).unwrap().is_none());
        assert!(acquire_file_lock(&lock_path, false, false).unwrap().is_none());
        drop(exclusive);

        let shared_a = acquire_file_lock(&lock_path, false, false).unwrap();
        let shared_b = acquire_file_lock(&lock_path, false, false).unwrap();
        assert!(shared_a.is_some());
        assert!(shared_b.is_some());
        assert!(acquire_file_lock(&lock_path, true, false).unwrap().is_none());
        drop(shared_a);
        assert!(acquire_file_lock(&lock_path, true, false).unwrap().is_none());
        drop(shared_b);

        assert!(acquire_file_lock(&lock_path, true, false).unwrap().is_some());
        assert!(lock_path.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_set_workspace_download_dest_preserves_valid_json_and_leaves_no_tmp() {
        let root = temp_dir("concurrent-dest");
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();

        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();

        let thread_count = 8usize;
        let expected_dests: Vec<PathBuf> = (0..thread_count)
            .map(|i| {
                let d = root.join(format!("dest-{i}"));
                fs::create_dir_all(&d).unwrap();
                normalize_canonical_path(&d).unwrap()
            })
            .collect();

        std::thread::scope(|scope| {
            for i in 0..thread_count {
                let paths = &paths;
                let root = &root;
                scope.spawn(move || {
                    for _ in 0..5 {
                        let res = set_workspace_download_dest_with_cwd(
                            paths,
                            Path::new(&format!("dest-{i}")),
                            root,
                        );
                        assert!(res.is_ok(), "writer thread {i} failed: {res:?}");
                        let cfg = load_workspace_config(paths).unwrap();
                        assert!(cfg.download_dest.is_some());
                    }
                });
            }
        });

        let final_cfg = load_workspace_config(&paths).unwrap();
        assert_eq!(final_cfg.version, WORKSPACE_CONFIG_VERSION);
        assert!(
            final_cfg
                .download_dest
                .as_ref()
                .is_some_and(|d| expected_dests.contains(d)),
            "unexpected download_dest: {:?}",
            final_cfg.download_dest
        );

        let config_dir = paths.config_path.parent().unwrap();
        let leftover_tmps: Vec<PathBuf> = fs::read_dir(config_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tmp"))
            })
            .collect();
        assert!(leftover_tmps.is_empty(), "leftover tmp files: {leftover_tmps:?}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_init_and_resolve_never_corrupt_or_leave_tmp() {
        let root = temp_dir("concurrent-init");
        let cfg_home = root.join("xdg_config");
        let home = root.join("home");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(home.clone()),
        )
        .unwrap();

        std::thread::scope(|scope| {
            for _ in 0..4 {
                let ws = &ws;
                let root = &root;
                let cfg_home = &cfg_home;
                let home = &home;
                scope.spawn(move || {
                    for _ in 0..4 {
                        initialize_workspace_with_env(
                            ws,
                            root,
                            Some(cfg_home.clone().into_os_string()),
                            Some(home.clone()),
                        )
                        .unwrap();
                    }
                });
            }
            for _ in 0..4 {
                let cfg_home = &cfg_home;
                let home = &home;
                let run_dir = &run_dir;
                scope.spawn(move || {
                    for _ in 0..8 {
                        let resolved = AppPaths::resolve_from_env(
                            Some(cfg_home.clone().into_os_string()),
                            Some(home.clone()),
                            Some(run_dir.clone().into_os_string()),
                            "1000",
                        )
                        .unwrap();
                        assert_eq!(resolved.config_path, resolved.working_dir.join("config/config.json"));
                    }
                });
            }
        });

        for dir in [cfg_home.join("termusic"), normalize_canonical_path(&ws).unwrap().join("config")] {
            let leftover_tmps: Vec<PathBuf> = fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok().map(|entry| entry.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".tmp"))
                })
                .collect();
            assert!(leftover_tmps.is_empty(), "leftover tmp files in {dir:?}: {leftover_tmps:?}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ipc_endpoint_from_override_rejects_blank_and_maps_per_platform() {
        assert!(matches!(
            IpcEndpoint::from_override("   "),
            Err(TermusicError::InvalidArg(_))
        ));

        #[cfg(unix)]
        {
            let ep = IpcEndpoint::from_override("/tmp/custom.sock").unwrap();
            assert_eq!(ep, IpcEndpoint::UnixSocket(PathBuf::from("/tmp/custom.sock")));
            assert_eq!(ep.to_string(), "/tmp/custom.sock");
        }

        #[cfg(windows)]
        {
            let explicit = IpcEndpoint::from_override(r"\\.\pipe\custom-termusic").unwrap();
            assert_eq!(
                explicit,
                IpcEndpoint::NamedPipe(r"\\.\pipe\custom-termusic".to_string())
            );
            let mapped = IpcEndpoint::from_override(r"C:\tmp\custom.sock").unwrap();
            assert!(matches!(
                mapped,
                IpcEndpoint::NamedPipe(ref name) if name.starts_with(r"\\.\pipe\termusic-")
            ));
        }
    }

    #[test]
    fn ensure_dirs_creates_library_and_runtime_parents() {
        let root = temp_dir("ensure-dirs");
        let data_dir = root.join("data");
        let run_dir = root.join("termusic-run");
        let paths = AppPaths::from_roots(&data_dir, &run_dir);
        paths.ensure_dirs().unwrap();
        assert!(data_dir.is_dir());
        assert!(run_dir.is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_pointer_prefers_xdg_then_platform_defaults() {
        let xdg = PathBuf::from("/custom/xdg");
        let home = PathBuf::from("/custom/home");
        let os_cfg = PathBuf::from("/custom/os-cfg");

        let from_xdg = resolve_workspace_pointer_path_with_os(
            Some(xdg.clone().into_os_string()),
            Some(home.clone()),
            Some(os_cfg.clone()),
        )
        .unwrap();
        assert_eq!(from_xdg, xdg.join("termusic").join("workspace.json"));

        let without_xdg =
            resolve_workspace_pointer_path_with_os(None, Some(home.clone()), Some(os_cfg.clone()))
                .unwrap();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(without_xdg, os_cfg.join("termusic").join("workspace.json"));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(
            without_xdg,
            home.join(".config").join("termusic").join("workspace.json")
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn termux_runtime_fallback_ladder_skips_slash_tmp() {
        let env = RuntimeEnv {
            is_termux: true,
            tmpdir: Some(OsString::from("/data/data/com.termux/files/usr/tmp")),
            prefix: Some(OsString::from("/data/data/com.termux/files/usr")),
            home_dir: Some(PathBuf::from("/data/data/com.termux/files/home")),
        };

        let (ep1, pid1, _) = resolve_runtime_paths_with_env(None, "10234", &env);
        assert_eq!(
            ep1,
            IpcEndpoint::UnixSocket(PathBuf::from(
                "/data/data/com.termux/files/usr/tmp/termusic-10234.sock"
            ))
        );
        assert_eq!(
            pid1,
            PathBuf::from("/data/data/com.termux/files/usr/tmp/termusic-10234.pid")
        );

        let no_tmpdir = RuntimeEnv {
            tmpdir: None,
            ..env.clone()
        };
        let (ep2, _, _) = resolve_runtime_paths_with_env(None, "10234", &no_tmpdir);
        assert_eq!(
            ep2,
            IpcEndpoint::UnixSocket(PathBuf::from(
                "/data/data/com.termux/files/usr/var/run/termusic.sock"
            ))
        );

        let only_home = RuntimeEnv {
            is_termux: true,
            tmpdir: None,
            prefix: None,
            home_dir: Some(PathBuf::from("/data/data/com.termux/files/home")),
        };
        let (ep3, _, _) = resolve_runtime_paths_with_env(None, "10234", &only_home);
        assert_eq!(
            ep3,
            IpcEndpoint::UnixSocket(PathBuf::from(
                "/data/data/com.termux/files/home/.cache/termusic/run/termusic.sock"
            ))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_long_xdg_runtime_dir_falls_back_to_short_tmp_socket() {
        let long_dir = format!("/var/folders/xx/{}/T", "a".repeat(90));
        let (ep, pid, _) = resolve_runtime_paths_with_env(
            Some(OsString::from(long_dir)),
            "501",
            &RuntimeEnv::default(),
        );
        assert_eq!(
            ep,
            IpcEndpoint::UnixSocket(PathBuf::from("/tmp/termusic-501/termusic.sock"))
        );
        assert_eq!(pid, PathBuf::from("/tmp/termusic-501/termusic.pid"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_paths_use_named_pipe_and_local_app_data() {
        let env = RuntimeEnv {
            home_dir: Some(PathBuf::from(r"C:\Users\alice")),
            local_app_data: Some(PathBuf::from(r"C:\Users\alice\AppData\Local")),
        };
        let (ep, pid, log) = resolve_runtime_paths_with_env(None, "S-1-5-21-1001", &env);
        assert_eq!(
            ep,
            IpcEndpoint::NamedPipe(r"\\.\pipe\termusic-S-1-5-21-1001".to_string())
        );
        assert_eq!(
            pid,
            PathBuf::from(r"C:\Users\alice\AppData\Local\termusic\run\termusic.pid")
        );
        assert_eq!(
            log,
            PathBuf::from(r"C:\Users\alice\AppData\Local\termusic\run\termusic-daemon.log")
        );
    }

    #[test]
    fn legacy_workspace_config_without_search_limit_deserializes_as_none() {
        let legacy_json = r#"{
  "version": 1,
  "working_dir": "/tmp/ws",
  "config_dir": "/tmp/ws/config",
  "playlists_dir": "/tmp/ws/playlists",
  "music_dir": "/tmp/ws/music",
  "download_dest": null
}"#;
        let cfg: WorkspaceConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(cfg.search_limit, None);

        let minimal_json = r#"{
  "version": 1,
  "working_dir": "/tmp/ws",
  "config_dir": "/tmp/ws/config",
  "playlists_dir": "/tmp/ws/playlists",
  "music_dir": "/tmp/ws/music"
}"#;
        let cfg2: WorkspaceConfig = serde_json::from_str(minimal_json).unwrap();
        assert_eq!(cfg2.download_dest, None);
        assert_eq!(cfg2.search_limit, None);
        assert_eq!(cfg2.search_provider, None);
        assert_eq!(cfg2.search_format, None);
        assert_eq!(cfg.search_provider, None);
        assert_eq!(cfg.search_format, None);
    }

    #[test]
    fn set_workspace_search_provider_and_format_persist_roundtrip() {
        let root = temp_dir("search-provider-format");
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        let init_cfg = initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();
        assert_eq!(init_cfg.search_provider, None);
        assert_eq!(init_cfg.search_format, None);

        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();

        assert_eq!(
            set_workspace_search_provider(&paths, SearchProvider::SoundCloud).unwrap(),
            SearchProvider::SoundCloud
        );
        assert_eq!(
            set_workspace_search_format(&paths, DownloadFormat::Mp4).unwrap(),
            DownloadFormat::Mp4
        );
        assert_eq!(set_workspace_search_limit(&paths, 15).unwrap(), 15);

        let reloaded = load_workspace_config(&paths).unwrap();
        assert_eq!(reloaded.search_provider, Some(SearchProvider::SoundCloud));
        assert_eq!(reloaded.search_format, Some(DownloadFormat::Mp4));
        assert_eq!(reloaded.search_limit, Some(15));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_search_limit_enforces_bounds() {
        assert!(matches!(
            validate_search_limit(0),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            validate_search_limit(MAX_SEARCH_LIMIT + 1),
            Err(TermusicError::InvalidArg(_))
        ));
        assert_eq!(validate_search_limit(1).unwrap(), 1);
        assert_eq!(
            validate_search_limit(DEFAULT_SEARCH_LIMIT).unwrap(),
            DEFAULT_SEARCH_LIMIT
        );
        assert_eq!(
            validate_search_limit(MAX_SEARCH_LIMIT).unwrap(),
            MAX_SEARCH_LIMIT
        );
    }

    #[test]
    fn set_workspace_search_limit_persists_and_validates() {
        let root = temp_dir("search-limit");
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        let init_cfg = initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();
        assert_eq!(init_cfg.search_limit, None);

        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();
        assert_eq!(load_workspace_config(&paths).unwrap().search_limit, None);

        assert!(matches!(
            set_workspace_search_limit(&paths, 0),
            Err(TermusicError::InvalidArg(_))
        ));
        assert!(matches!(
            set_workspace_search_limit(&paths, MAX_SEARCH_LIMIT + 1),
            Err(TermusicError::InvalidArg(_))
        ));
        assert_eq!(load_workspace_config(&paths).unwrap().search_limit, None);

        let saved_dest =
            set_workspace_download_dest_with_cwd(&paths, Path::new("downloads"), &root).unwrap();
        assert_eq!(set_workspace_search_limit(&paths, 25).unwrap(), 25);

        let reloaded = load_workspace_config(&paths).unwrap();
        assert_eq!(reloaded.search_limit, Some(25));
        assert_eq!(reloaded.download_dest, Some(saved_dest));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_set_workspace_search_limit_preserves_valid_json_and_leaves_no_tmp() {
        let root = temp_dir("concurrent-search-limit");
        let cfg_home = root.join("xdg_config");
        let run_dir = root.join("run");
        let ws = root.join("ws");

        initialize_workspace_with_env(
            &ws,
            &root,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();

        let paths = AppPaths::resolve_from_env(
            Some(cfg_home.into_os_string()),
            Some(root.join("home")),
            Some(run_dir.into_os_string()),
            "1000",
        )
        .unwrap();

        let thread_count = 8usize;
        let expected_limits: Vec<usize> = (1..=thread_count).map(|i| i * 5).collect();

        std::thread::scope(|scope| {
            for &limit in &expected_limits {
                let paths = &paths;
                scope.spawn(move || {
                    for _ in 0..5 {
                        let res = set_workspace_search_limit(paths, limit);
                        assert!(res.is_ok(), "writer thread limit={limit} failed: {res:?}");
                        let cfg = load_workspace_config(paths).unwrap();
                        assert!(cfg.search_limit.is_some());
                    }
                });
            }
        });

        let final_cfg = load_workspace_config(&paths).unwrap();
        assert_eq!(final_cfg.version, WORKSPACE_CONFIG_VERSION);
        assert!(
            final_cfg
                .search_limit
                .is_some_and(|l| expected_limits.contains(&l)),
            "unexpected search_limit: {:?}",
            final_cfg.search_limit
        );

        let config_dir = paths.config_path.parent().unwrap();
        let leftover_tmps: Vec<PathBuf> = fs::read_dir(config_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tmp"))
            })
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "leftover tmp files: {leftover_tmps:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
