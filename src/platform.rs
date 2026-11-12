use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Result;

/// Cross-platform shutdown signal listener (`SIGINT` / `SIGTERM` on Unix; console control
/// events on Windows).
#[derive(Debug)]
pub struct ShutdownSignals {
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
    #[cfg(windows)]
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
}

impl ShutdownSignals {
    /// Register OS shutdown signal listeners for the current platform.
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                sigint: signal(SignalKind::interrupt())?,
                sigterm: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
            Ok(Self {
                ctrl_c: ctrl_c()?,
                ctrl_break: ctrl_break()?,
                ctrl_close: ctrl_close()?,
                ctrl_shutdown: ctrl_shutdown()?,
            })
        }
    }

    /// Wait for the next shutdown signal and return a short signal label.
    ///
    /// This method is cancel-safe and suitable for direct use inside `tokio::select!`.
    pub async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.sigint.recv() => "SIGINT",
                _ = self.sigterm.recv() => "SIGTERM",
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                _ = self.ctrl_c.recv() => "CTRL_C",
                _ = self.ctrl_break.recv() => "CTRL_BREAK",
                _ = self.ctrl_close.recv() => "CTRL_CLOSE",
                _ = self.ctrl_shutdown.recv() => "CTRL_SHUTDOWN",
            }
        }
    }
}

/// Canonicalize `path`, stripping Windows `\\?\` verbatim prefixes when safe.
pub fn canonicalize_clean(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

/// Return a stable, path-safe per-user identifier for scoping IPC endpoints and runtime files.
pub fn user_runtime_id() -> String {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` is always safe to call and cannot fail.
        unsafe { libc::geteuid() }.to_string()
    }
    #[cfg(windows)]
    {
        windows_current_user_sid().unwrap_or_else(fallback_windows_user_id)
    }
}

#[cfg(windows)]
struct HandleGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an owned non-null Win32 handle.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
fn windows_current_user_sid() -> Option<String> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, FALSE, GetLastError, HLOCAL, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct LocalAllocGuard(windows_sys::core::PWSTR);
    impl Drop for LocalAllocGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was allocated by `ConvertSidToStringSidW` and must be freed via `LocalFree`.
                unsafe {
                    LocalFree(self.0.cast::<c_void>() as HLOCAL);
                }
            }
        }
    }

    let mut token = null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `token` receives an owned handle on success.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == FALSE || token.is_null() {
        return None;
    }
    let _token_guard = HandleGuard(token);

    let mut needed: u32 = 0;
    // SAFETY: Querying required `TOKEN_USER` buffer size with null buffer and zero length.
    let probed = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
    if probed != FALSE || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
        return None;
    }

    let word_bytes = std::mem::size_of::<usize>();
    let min_words = std::mem::size_of::<TOKEN_USER>().div_ceil(word_bytes);
    let needed_words = (needed as usize).div_ceil(word_bytes);
    let mut buf = vec![0usize; needed_words.max(min_words)];
    let buf_len = u32::try_from(buf.len() * word_bytes).ok()?;
    let mut returned: u32 = 0;
    // SAFETY: `buf` is `usize`-aligned (matching `TOKEN_USER` pointer alignment) and `buf_len` matches its byte capacity.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            buf_len,
            &mut returned,
        )
    };
    if ok == FALSE || (returned as usize) < std::mem::size_of::<TOKEN_USER>() {
        return None;
    }

    // SAFETY: `GetTokenInformation(TokenUser, ...)` succeeded and wrote `TOKEN_USER` at the start of `buf`.
    let token_user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    if token_user.User.Sid.is_null() {
        return None;
    }

    let mut sid_wide: windows_sys::core::PWSTR = null_mut();
    // SAFETY: `token_user.User.Sid` points inside `buf` and `sid_wide` receives a NUL-terminated UTF-16 SID string.
    let converted = unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_wide) };
    if converted == FALSE || sid_wide.is_null() {
        return None;
    }
    let _sid_guard = LocalAllocGuard(sid_wide);

    let mut len = 0usize;
    // SAFETY: `ConvertSidToStringSidW` guarantees NUL termination within standard SID bounds.
    while len < 512 && unsafe { *sid_wide.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `sid_wide` points to `len` initialized UTF-16 code units.
    let slice = unsafe { std::slice::from_raw_parts(sid_wide, len) };
    let sid = String::from_utf16(slice).ok()?;
    (!sid.is_empty()).then_some(sid)
}

#[cfg(windows)]
fn fallback_windows_user_id() -> String {
    let username = std::env::var("USERNAME").ok().or_else(|| std::env::var("USER").ok());
    let domain = std::env::var("USERDOMAIN").ok();
    let profile = std::env::var("USERPROFILE").ok();
    fallback_windows_user_id_from(username.as_deref(), domain.as_deref(), profile.as_deref())
}

#[cfg(any(windows, test))]
fn fallback_windows_user_id_from(
    username: Option<&str>,
    domain: Option<&str>,
    profile: Option<&str>,
) -> String {
    let raw_user = username.unwrap_or("user");
    let sanitized: String = raw_user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let prefix = if sanitized.is_empty() {
        "user"
    } else {
        sanitized.as_str()
    };

    let mut hash_input = String::new();
    if let Some(d) = domain {
        hash_input.push_str(d);
    }
    hash_input.push('\0');
    if let Some(u) = username {
        hash_input.push_str(u);
    }
    hash_input.push('\0');
    if let Some(p) = profile {
        hash_input.push_str(p);
    }

    let hash = fnv1a_64(hash_input.as_bytes());
    format!("{prefix}-{hash:016x}")
}

#[cfg(any(windows, test))]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Configure `cmd` to run detached in its own session / process group without a console window.
pub fn configure_detached_daemon(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // SAFETY: `setsid` is async-signal-safe and detaches the spawned daemon into a new session.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Return `true` if a process with `pid` currently appears alive.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(raw_pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: `kill(pid, 0)` probes process existence without delivering a signal.
        let rc = unsafe { libc::kill(raw_pid, 0) };
        if rc == 0 {
            return true;
        }
        matches!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{FALSE, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: Opening process handle solely to query exit status.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if handle.is_null() {
            return false;
        }
        let _guard = HandleGuard(handle);
        let mut exit_code: u32 = 0;
        // SAFETY: `handle` is an open process handle and `exit_code` is a valid out pointer.
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        ok != FALSE && exit_code == STILL_ACTIVE as u32
    }
}

/// Ensure `path` exists as a directory, applying `0o700` permissions on Unix.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Restrict Unix domain socket permissions to owner-only (`0o600`); no-op on Windows.
pub fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Return `true` when running inside Termux (`TERMUX_VERSION` set or `PREFIX` containing `"com.termux"`).
pub fn is_termux() -> bool {
    is_termux_from_env(
        std::env::var_os("TERMUX_VERSION").as_deref(),
        std::env::var_os("PREFIX").as_deref(),
    )
}

fn is_termux_from_env(termux_version: Option<&OsStr>, prefix: Option<&OsStr>) -> bool {
    termux_version.is_some_and(|v| !v.is_empty())
        || prefix.is_some_and(|p| p.to_string_lossy().contains("com.termux"))
}

/// Resolve the user's default music directory via `dirs::audio_dir()`, falling back to
/// `$HOME/storage/music`, `/sdcard/Music`, and `$HOME/Music` if they exist.
pub fn default_music_dir() -> Option<PathBuf> {
    let home_dir = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    default_music_dir_with(dirs::audio_dir(), home_dir.as_deref(), Path::new("/sdcard/Music"))
}

fn default_music_dir_with(
    dirs_audio: Option<PathBuf>,
    home_dir: Option<&Path>,
    sdcard_music: &Path,
) -> Option<PathBuf> {
    if let Some(dir) = dirs_audio {
        return Some(dir);
    }

    if let Some(home) = home_dir {
        let termux_music = home.join("storage").join("music");
        if termux_music.is_dir() {
            return Some(termux_music);
        }
    }

    if sdcard_music.is_dir() {
        return Some(sdcard_music.to_path_buf());
    }

    if let Some(home) = home_dir {
        let home_music = home.join("Music");
        if home_music.is_dir() {
            return Some(home_music);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-platform-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn canonicalize_clean_resolves_relative_segments() {
        let root = temp_dir("canon");
        let nested = root.join("alpha").join("beta");
        fs::create_dir_all(&nested).unwrap();

        let messy = root.join("alpha").join("..").join("alpha").join("beta");
        let cleaned = canonicalize_clean(&messy).unwrap();
        let direct = canonicalize_clean(&nested).unwrap();
        assert_eq!(cleaned, direct);
        assert!(cleaned.is_absolute());

        #[cfg(windows)]
        assert!(!cleaned.to_string_lossy().starts_with(r"\\?\"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn user_runtime_id_is_non_empty_and_path_safe() {
        let id = user_runtime_id();
        assert!(!id.is_empty());
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "unexpected characters in user_runtime_id: {id}"
        );

        #[cfg(unix)]
        {
            let expected = unsafe { libc::geteuid() }.to_string();
            assert_eq!(id, expected);
        }
    }

    #[test]
    fn fallback_windows_user_id_sanitizes_and_hashes_deterministically() {
        let a = fallback_windows_user_id_from(
            Some("pat/rick@work"),
            Some("CORP"),
            Some(r"C:\Users\pat"),
        );
        let b = fallback_windows_user_id_from(
            Some("pat/rick@work"),
            Some("CORP"),
            Some(r"C:\Users\pat"),
        );
        let c = fallback_windows_user_id_from(
            Some("pat/rick@work"),
            Some("OTHER"),
            Some(r"C:\Users\pat"),
        );
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("pat_rick_work-"));
        assert!(a.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));

        let empty = fallback_windows_user_id_from(Some(""), None, None);
        assert!(empty.starts_with("user-"));
    }

    #[test]
    fn is_pid_alive_distinguishes_current_and_exited_process() {
        assert!(!is_pid_alive(0));
        assert!(is_pid_alive(std::process::id()));

        #[cfg(unix)]
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap();

        let child_pid = child.id();
        let status = child.wait().unwrap();
        assert!(status.success());
        assert!(!is_pid_alive(child_pid));
    }

    #[test]
    fn ensure_private_dir_and_socket_permissions_behave_as_expected() {
        let root = temp_dir("perms");
        let private = root.join("nested").join("run");
        ensure_private_dir(&private).unwrap();
        assert!(private.is_dir());

        let file_path = private.join("termusic.sock");
        fs::write(&file_path, b"placeholder").unwrap();
        restrict_socket_permissions(&file_path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir_mode = fs::metadata(&private).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);

            let sock_mode = fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(sock_mode, 0o600);

            // Re-running tightens looser existing directory permissions back to 0o700.
            fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
            ensure_private_dir(&private).unwrap();
            let tightened_mode = fs::metadata(&private).unwrap().permissions().mode() & 0o777;
            assert_eq!(tightened_mode, 0o700);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn is_termux_from_env_checks_version_and_prefix() {
        assert!(!is_termux_from_env(None, None));
        assert!(!is_termux_from_env(
            Some(OsStr::new("")),
            Some(OsStr::new("/usr/local"))
        ));
        assert!(is_termux_from_env(Some(OsStr::new("0.118.0")), None));
        assert!(is_termux_from_env(
            None,
            Some(OsStr::new("/data/data/com.termux/files/usr"))
        ));
        assert!(!is_termux_from_env(
            None,
            Some(OsStr::new("/home/user/.local"))
        ));
        let _ = is_termux();
    }

    #[test]
    fn default_music_dir_with_respects_fallback_precedence() {
        let root = temp_dir("music-dir");
        let xdg_music = root.join("xdg_music");
        let home = root.join("home");
        let termux_music = home.join("storage").join("music");
        let sdcard_music = root.join("sdcard").join("Music");
        let home_music = home.join("Music");
        let missing_sdcard = root.join("missing_sdcard");

        // Explicit dirs::audio_dir() takes precedence when available.
        assert_eq!(
            default_music_dir_with(Some(xdg_music.clone()), Some(&home), &missing_sdcard),
            Some(xdg_music)
        );

        // No existing fallback directories -> None.
        assert_eq!(
            default_music_dir_with(None, Some(&home), &missing_sdcard),
            None
        );

        // Lowest-priority fallback: $HOME/Music.
        fs::create_dir_all(&home_music).unwrap();
        assert_eq!(
            default_music_dir_with(None, Some(&home), &missing_sdcard),
            Some(home_music.clone())
        );

        // Middle fallback: /sdcard/Music beats $HOME/Music.
        fs::create_dir_all(&sdcard_music).unwrap();
        assert_eq!(
            default_music_dir_with(None, Some(&home), &sdcard_music),
            Some(sdcard_music.clone())
        );

        // Highest fallback: $HOME/storage/music beats /sdcard/Music.
        fs::create_dir_all(&termux_music).unwrap();
        assert_eq!(
            default_music_dir_with(None, Some(&home), &sdcard_music),
            Some(termux_music)
        );

        let _ = default_music_dir();
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn shutdown_signals_initializes_and_times_out_when_idle() {
        let mut signals = ShutdownSignals::new().unwrap();
        let timed_out = tokio::time::timeout(Duration::from_millis(10), signals.recv()).await;
        assert!(timed_out.is_err());
    }

    #[tokio::test]
    async fn configure_detached_daemon_spawns_clean_child() {
        #[cfg(unix)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", "exit 0"]);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        };

        configure_detached_daemon(&mut cmd);
        let status = cmd.status().await.unwrap();
        assert!(status.success());
    }
}
