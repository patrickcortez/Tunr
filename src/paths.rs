use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TermusicError};

pub const WORKSPACE_CONFIG_VERSION: u32 = 1;

const NOT_INITIALIZED_MSG: &str =
    "Termusic is not initialized. Run `termusic init <working-directory-location>` first.";

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
}

/// Bootstrap pointer persisted at `$XDG_CONFIG_HOME/termusic/workspace.json`.
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
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_path: PathBuf,
}

impl AppPaths {
    /// Construct paths rooted at explicit `data_dir` and `runtime_dir` (for isolated tests).
    pub fn from_roots(data_dir: impl AsRef<Path>, runtime_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref();
        let runtime_dir = runtime_dir.as_ref();
        Self {
            working_dir: data_dir.to_path_buf(),
            config_path: data_dir.join("config").join("config.json"),
            playlists_dir: data_dir.join("playlists"),
            music_dir: data_dir.join("music"),
            library_path: data_dir.join("library.json"),
            socket_path: runtime_dir.join("termusic.sock"),
            pid_path: runtime_dir.join("termusic.pid"),
            log_path: runtime_dir.join("termusic-daemon.log"),
        }
    }

    /// Resolve workspace and runtime paths for the current user environment.
    /// Fails with `TermusicError::NotInitialized` if `termusic init` has not been run.
    pub fn resolve() -> Result<Self> {
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let home_dir = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
        let uid = unsafe { libc::geteuid() };

        Self::resolve_from_env(xdg_config_home, home_dir, xdg_runtime_dir, uid)
    }

    pub fn resolve_from_env(
        xdg_config_home: Option<OsString>,
        home_dir: Option<PathBuf>,
        xdg_runtime_dir: Option<OsString>,
        uid: libc::uid_t,
    ) -> Result<Self> {
        let pointer_path = resolve_workspace_pointer_path(xdg_config_home, home_dir)?;
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

        let cfg_raw = fs::read_to_string(&config_path)?;
        let cfg: WorkspaceConfig = serde_json::from_str(&cfg_raw)?;

        let (socket_path, pid_path, log_path) = resolve_runtime_paths(xdg_runtime_dir, uid);
        Ok(Self {
            working_dir: cfg.working_dir,
            config_path: cfg.config_dir.join("config.json"),
            library_path: cfg.playlists_dir.join("library.json"),
            playlists_dir: cfg.playlists_dir,
            music_dir: cfg.music_dir,
            socket_path,
            pid_path,
            log_path,
        })
    }

    /// Ensure parent directories exist for library and runtime files.
    pub fn ensure_dirs(&self) -> Result<()> {
        for path in [
            &self.library_path,
            &self.socket_path,
            &self.pid_path,
            &self.log_path,
        ] {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    }
}

/// Initialize a Termusic working directory and persist the bootstrap workspace pointer.
pub fn initialize_workspace(dir: &Path) -> Result<WorkspaceConfig> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home_dir = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let current_dir = std::env::current_dir()?;
    initialize_workspace_with_env(dir, &current_dir, xdg_config_home, home_dir)
}

/// Environment-parameterized workspace initialization helper (for isolated unit tests).
pub fn initialize_workspace_with_env(
    dir: &Path,
    current_dir: &Path,
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
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
    let working_dir = fs::canonicalize(&candidate)?;

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
    };

    let config_file = config_dir.join("config.json");
    write_json_atomic(&config_file, &workspace_cfg)?;

    let pointer_path = resolve_workspace_pointer_path(xdg_config_home, home_dir)?;
    let pointer = WorkspacePointer { working_dir };
    write_json_atomic(&pointer_path, &pointer)?;

    Ok(workspace_cfg)
}

/// Load and deserialize `WorkspaceConfig` from `paths.config_path`.
pub fn load_workspace_config(paths: &AppPaths) -> Result<WorkspaceConfig> {
    let raw = fs::read_to_string(&paths.config_path)?;
    let cfg: WorkspaceConfig = serde_json::from_str(&raw)?;
    Ok(cfg)
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
    let abs = fs::canonicalize(&candidate)?;

    let mut cfg = load_workspace_config(paths)?;
    cfg.download_dest = Some(abs.clone());
    write_json_atomic(&paths.config_path, &cfg)?;
    Ok(abs)
}

fn resolve_workspace_pointer_path(
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    let config_root = match xdg_config_home.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => match home_dir.filter(|p| !p.as_os_str().is_empty()) {
            Some(home) => home.join(".config"),
            None => match dirs::config_dir() {
                Some(dir) => dir,
                None => {
                    return Err(TermusicError::Io(io::Error::new(
                        io::ErrorKind::NotFound,
                        "could not determine XDG_CONFIG_HOME or home directory",
                    )));
                }
            },
        },
    };
    Ok(config_root.join("termusic").join("workspace.json"))
}

fn resolve_runtime_paths(
    xdg_runtime_dir: Option<OsString>,
    uid: libc::uid_t,
) -> (PathBuf, PathBuf, PathBuf) {
    if let Some(runtime_dir) = xdg_runtime_dir.filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(runtime_dir);
        return (
            dir.join("termusic.sock"),
            dir.join("termusic.pid"),
            dir.join("termusic-daemon.log"),
        );
    }

    let tmp = PathBuf::from("/tmp");
    (
        tmp.join(format!("termusic-{uid}.sock")),
        tmp.join(format!("termusic-{uid}.pid")),
        tmp.join(format!("termusic-{uid}-daemon.log")),
    )
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => path.with_file_name(format!("{name}.tmp")),
        None => path.with_extension("json.tmp"),
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
        assert_eq!(paths.socket_path, PathBuf::from("/tmp/run/termusic.sock"));
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
            1000,
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
            1000,
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
        let run_dir = root.join("run");
        fs::create_dir_all(&cwd).unwrap();

        let cfg = initialize_workspace_with_env(
            Path::new("my-ws"),
            &cwd,
            Some(cfg_home.clone().into_os_string()),
            Some(root.join("home")),
        )
        .unwrap();

        let canonical_ws = fs::canonicalize(cwd.join("my-ws")).unwrap();
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
            1000,
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
        assert_eq!(paths.socket_path, run_dir.join("termusic.sock"));
        assert_eq!(paths.pid_path, run_dir.join("termusic.pid"));
        assert_eq!(paths.log_path, run_dir.join("termusic-daemon.log"));

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

        let canonical_ws = fs::canonicalize(&ws).unwrap();
        assert!(home.join(".config/termusic/workspace.json").exists());

        let paths = AppPaths::resolve_from_env(
            Some(OsString::from("")),
            Some(home),
            None,
            1042,
        )
        .unwrap();

        assert_eq!(paths.working_dir, canonical_ws);
        assert_eq!(
            paths.library_path,
            canonical_ws.join("playlists/library.json")
        );
        assert_eq!(paths.socket_path, PathBuf::from("/tmp/termusic-1042.sock"));
        assert_eq!(paths.pid_path, PathBuf::from("/tmp/termusic-1042.pid"));
        assert_eq!(
            paths.log_path,
            PathBuf::from("/tmp/termusic-1042-daemon.log")
        );

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
            1000,
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
        let expected = fs::canonicalize(root.join("downloads/sub")).unwrap();
        assert_eq!(saved, expected);
        assert!(expected.is_dir());

        let reloaded = load_workspace_config(&paths).unwrap();
        assert_eq!(reloaded.download_dest, Some(expected));

        let _ = fs::remove_dir_all(&root);
    }
}
