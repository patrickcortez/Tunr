use std::fmt;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::process::{Child, Command};

use crate::error::{Result, TunrError};
use crate::library::Song;
use crate::paths::{self, AppPaths, IpcEndpoint};
pub use crate::platform::is_pid_alive;

const IPC_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DAEMON_SPAWN_MAX_ATTEMPTS: usize = 80; // 80 * 25ms = 2.0s
pub const MAX_IPC_MESSAGE_BYTES: usize = 1024 * 1024; // 1 MiB line frame limit

/// Queue playback ordering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum PlayMode {
    #[default]
    Next,
    Shuffle,
}

impl fmt::Display for PlayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayMode::Next => f.write_str("next"),
            PlayMode::Shuffle => f.write_str("shuffle"),
        }
    }
}

/// Current state of the audio playback engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaybackState::Stopped => f.write_str("stopped"),
            PlaybackState::Playing => f.write_str("playing"),
            PlaybackState::Paused => f.write_str("paused"),
        }
    }
}

/// Disambiguation hint when a target title may match a song or playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    #[default]
    Auto,
    Song,
    Playlist,
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetKind::Auto => f.write_str("auto"),
            TargetKind::Song => f.write_str("song"),
            TargetKind::Playlist => f.write_str("playlist"),
        }
    }
}

/// Requests sent from CLI/TUI clients to the Tunr playback daemon over Unix socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcRequest {
    Play {
        queue: Vec<Song>,
        start_index: Option<usize>,
        mode: PlayMode,
        playlist_name: Option<String>,
        #[serde(default)]
        loop_playback: bool,
    },
    Next,
    Prev,
    SeekRelative(f64),
    SeekAbsolute(f64),
    TogglePause,
    SetPaused(bool),
    SetVolume(u8),
    Stop,
    GetStatus,
    Shutdown,
}

fn default_volume() -> u8 {
    crate::paths::DEFAULT_VOLUME
}

/// Snapshot of current daemon playback state returned in `IpcResponse::Ok`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackStatus {
    pub state: PlaybackState,
    pub mode: PlayMode,
    pub playlist_name: Option<String>,
    pub current_track: Option<Song>,
    pub queue_index: usize,
    pub queue_len: usize,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub queue: Vec<Song>,
    #[serde(default)]
    pub loop_playback: bool,
    #[serde(default = "default_volume")]
    pub volume: u8,
}

impl Default for PlaybackStatus {
    fn default() -> Self {
        Self {
            state: PlaybackState::default(),
            mode: PlayMode::default(),
            playlist_name: None,
            current_track: None,
            queue_index: 0,
            queue_len: 0,
            position_secs: 0.0,
            duration_secs: None,
            queue: Vec::new(),
            loop_playback: false,
            volume: crate::paths::DEFAULT_VOLUME,
        }
    }
}

/// Response returned by the Tunr playback daemon for an `IpcRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcResponse {
    Ok(PlaybackStatus),
    Err(String),
}

/// Write a single newline-delimited JSON message to an async writer (e.g. `tokio::net::UnixStream`).
pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let mut encoded = serde_json::to_vec(msg)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single newline-delimited JSON message from an async buffered reader, capped at
/// [`MAX_IPC_MESSAGE_BYTES`] (1 MiB).
///
/// Returns `Ok(None)` cleanly if the peer closed the stream before sending any bytes.
pub async fn read_message<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncBufRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut buf = Vec::new();
    let bytes = {
        let mut limited = (&mut *reader).take((MAX_IPC_MESSAGE_BYTES + 1) as u64);
        limited.read_until(b'\n', &mut buf).await?
    };
    if bytes == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_IPC_MESSAGE_BYTES {
        return Err(TunrError::Ipc(format!(
            "IPC message exceeded {MAX_IPC_MESSAGE_BYTES} byte limit"
        )));
    }

    let text = std::str::from_utf8(&buf)
        .map_err(|err| TunrError::Ipc(format!("IPC message is not valid UTF-8: {err}")))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(TunrError::Ipc(
            "received empty IPC message frame".to_string(),
        ));
    }
    let parsed = serde_json::from_str::<T>(trimmed)?;
    Ok(Some(parsed))
}

/// Cross-platform async IPC stream wrapping Unix domain sockets or Windows named pipes.
pub enum IpcStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl IpcStream {
    /// Connect as a client to `endpoint`, returning `TunrError::Ipc` if the daemon is offline.
    pub async fn connect(endpoint: &IpcEndpoint) -> Result<Self> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::UnixSocket(socket_path) => {
                let stream = tokio::net::UnixStream::connect(socket_path)
                    .await
                    .map_err(|err| match err.kind() {
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                            TunrError::Ipc(format!(
                                "playback daemon is not running at {}: {err}",
                                socket_path.display()
                            ))
                        }
                        _ => TunrError::Io(err),
                    })?;
                Ok(Self::Unix(stream))
            }
            #[cfg(windows)]
            IpcEndpoint::NamedPipe(pipe_name) => {
                let deadline = tokio::time::Instant::now() + IPC_TIMEOUT;
                loop {
                    match tokio::net::windows::named_pipe::ClientOptions::new().open(pipe_name) {
                        Ok(client) => return Ok(Self::PipeClient(client)),
                        Err(err)
                            if err.raw_os_error()
                                == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32)
                                && tokio::time::Instant::now() < deadline =>
                        {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        Err(err)
                            if err.raw_os_error()
                                == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32) =>
                        {
                            return Err(TunrError::Ipc(format!(
                                "timed out waiting for busy named pipe at {pipe_name}: {err}"
                            )));
                        }
                        Err(err)
                            if err.kind() == io::ErrorKind::NotFound
                                || err.kind() == io::ErrorKind::ConnectionRefused =>
                        {
                            return Err(TunrError::Ipc(format!(
                                "playback daemon is not running at {pipe_name}: {err}"
                            )));
                        }
                        Err(err) => return Err(TunrError::Io(err)),
                    }
                }
            }
        }
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            Self::PipeServer(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            Self::PipeClient(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            Self::PipeServer(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            Self::PipeClient(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            Self::PipeServer(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            Self::PipeClient(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            Self::PipeServer(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            Self::PipeClient(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(windows)]
mod windows_pipe {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::null_mut;

    use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, FALSE, GetLastError, HANDLE, HLOCAL, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::IpcStream;
    use crate::error::{Result, TunrError};

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned non-null Win32 token handle.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalAllocGuard(*mut c_void);
    impl Drop for LocalAllocGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was allocated by a Win32 security API and must be freed via `LocalFree`.
                unsafe {
                    LocalFree(self.0 as HLOCAL);
                }
            }
        }
    }

    struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);

    // SAFETY: The underlying self-relative Win32 security descriptor is immutable after creation
    // and only passed read-only to `CreateNamedPipeW`, then freed on `Drop`.
    unsafe impl Send for OwnedSecurityDescriptor {}
    // SAFETY: Same immutability invariant as `Send`.
    unsafe impl Sync for OwnedSecurityDescriptor {}

    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
                unsafe {
                    LocalFree(self.0 as HLOCAL);
                }
            }
        }
    }

    fn current_user_sid_string() -> Option<String> {
        let mut token: HANDLE = null_mut();
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
        // SAFETY: `buf` is `usize`-aligned and sized for `TOKEN_USER`.
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

        // SAFETY: `GetTokenInformation` succeeded and populated `TOKEN_USER` at the start of `buf`.
        let token_user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
        if token_user.User.Sid.is_null() {
            return None;
        }

        let mut sid_wide: windows_sys::core::PWSTR = null_mut();
        // SAFETY: `token_user.User.Sid` points inside `buf`; `sid_wide` is freed via `LocalAllocGuard`.
        let converted = unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_wide) };
        if converted == FALSE || sid_wide.is_null() {
            return None;
        }
        let _sid_guard = LocalAllocGuard(sid_wide.cast::<c_void>());

        let mut len = 0usize;
        // SAFETY: `ConvertSidToStringSidW` NUL-terminates the SID string within standard SID bounds.
        while len < 512 && unsafe { *sid_wide.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `sid_wide` points to `len` initialized UTF-16 code units.
        let slice = unsafe { std::slice::from_raw_parts(sid_wide, len) };
        let sid = String::from_utf16(slice).ok()?;
        (!sid.is_empty()).then_some(sid)
    }

    impl OwnedSecurityDescriptor {
        fn current_user_only() -> io::Result<Self> {
            let sddl = match current_user_sid_string() {
                Some(sid) if sid.starts_with("S-1-") => {
                    format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})")
                }
                _ => "D:(A;;GA;;;CO)(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
            };
            let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            // SAFETY: `sddl_wide` is NUL-terminated UTF-16; `sd` receives an owned descriptor freed in `Drop`.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl_wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    null_mut(),
                )
            };
            if ok == FALSE || sd.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(sd))
        }
    }

    fn create_pipe_instance(
        pipe_name: &str,
        first: bool,
        sd: &OwnedSecurityDescriptor,
    ) -> io::Result<NamedPipeServer> {
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: FALSE,
        };
        let mut opts = ServerOptions::new();
        opts.pipe_mode(PipeMode::Byte)
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `attrs` points to a valid stack `SECURITY_ATTRIBUTES` whose descriptor stays valid
        // for the duration of `CreateNamedPipeW`.
        unsafe {
            opts.create_with_security_attributes_raw(
                pipe_name,
                (&mut attrs as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }

    pub(super) struct NamedPipeListener {
        pipe_name: String,
        sd: OwnedSecurityDescriptor,
        current: NamedPipeServer,
        standby: NamedPipeServer,
    }

    impl NamedPipeListener {
        pub(super) fn bind(pipe_name: &str) -> Result<Self> {
            let sd = OwnedSecurityDescriptor::current_user_only().map_err(|err| {
                TunrError::Ipc(format!(
                    "failed to build named pipe security descriptor: {err}"
                ))
            })?;
            let current = create_pipe_instance(pipe_name, true, &sd).map_err(|err| {
                TunrError::Ipc(format!("failed to bind named pipe {pipe_name}: {err}"))
            })?;
            let standby = create_pipe_instance(pipe_name, false, &sd).map_err(|err| {
                TunrError::Ipc(format!(
                    "failed to prepare standby named pipe {pipe_name}: {err}"
                ))
            })?;
            Ok(Self {
                pipe_name: pipe_name.to_string(),
                sd,
                current,
                standby,
            })
        }

        pub(super) async fn accept(&mut self) -> io::Result<IpcStream> {
            self.current.connect().await?;
            let replacement = create_pipe_instance(&self.pipe_name, false, &self.sd)?;
            let next_current = std::mem::replace(&mut self.standby, replacement);
            let connected = std::mem::replace(&mut self.current, next_current);
            Ok(IpcStream::PipeServer(connected))
        }
    }
}

/// Cross-platform async IPC server listener accepting incoming client streams.
pub struct IpcListener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: windows_pipe::NamedPipeListener,
}

impl IpcListener {
    /// Bind an IPC listener for `endpoint`, enforcing platform socket path length and permissions.
    pub fn bind(endpoint: &IpcEndpoint) -> Result<Self> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::UnixSocket(socket_path) => {
                #[cfg(target_os = "macos")]
                const MAX_SUN_PATH_BYTES: usize = 104;
                #[cfg(all(unix, not(target_os = "macos")))]
                const MAX_SUN_PATH_BYTES: usize = 108;

                let path_bytes = socket_path.as_os_str().as_encoded_bytes().len();
                if path_bytes >= MAX_SUN_PATH_BYTES {
                    return Err(TunrError::Ipc(format!(
                        "Unix socket path {} exceeds OS sockaddr_un limit ({path_bytes} >= {MAX_SUN_PATH_BYTES} bytes); set XDG_RUNTIME_DIR to a shorter directory",
                        socket_path.display()
                    )));
                }

                let inner = tokio::net::UnixListener::bind(socket_path).map_err(|err| {
                    TunrError::Ipc(format!(
                        "failed to bind playback daemon socket {}: {err}",
                        socket_path.display()
                    ))
                })?;
                crate::platform::restrict_socket_permissions(socket_path).map_err(|err| {
                    TunrError::Ipc(format!(
                        "failed to set socket permissions on {}: {err}",
                        socket_path.display()
                    ))
                })?;
                Ok(Self { inner })
            }
            #[cfg(windows)]
            IpcEndpoint::NamedPipe(pipe_name) => {
                let inner = windows_pipe::NamedPipeListener::bind(pipe_name)?;
                Ok(Self { inner })
            }
        }
    }

    /// Accept the next connected client stream.
    pub async fn accept(&mut self) -> io::Result<IpcStream> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            Ok(IpcStream::Unix(stream))
        }
        #[cfg(windows)]
        {
            self.inner.accept().await
        }
    }
}

fn read_pid_file(pid_path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(pid_path).ok()?;
    contents.trim().parse::<u32>().ok()
}

fn is_daemon_lock_held(lock_path: &Path) -> Result<bool> {
    Ok(paths::acquire_file_lock(lock_path, true, false)?.is_none())
}

async fn is_daemon_lock_held_async(lock_path: &Path) -> Result<bool> {
    let lock_path = lock_path.to_path_buf();
    tokio::task::spawn_blocking(move || is_daemon_lock_held(&lock_path))
        .await
        .map_err(|err| TunrError::Async(format!("daemon lock check task failed: {err}")))?
}

async fn exchange_on_stream(mut stream: IpcStream, req: &IpcRequest) -> Result<PlaybackStatus> {
    tokio::time::timeout(IPC_TIMEOUT, async {
        write_message(&mut stream, req).await?;

        let mut reader = BufReader::new(&mut stream);
        match read_message::<_, IpcResponse>(&mut reader).await? {
            Some(IpcResponse::Ok(status)) => Ok(status),
            Some(IpcResponse::Err(msg)) => Err(TunrError::Daemon(msg)),
            None => Err(TunrError::Ipc(
                "playback daemon closed connection without responding".to_string(),
            )),
        }
    })
    .await
    .map_err(|_| {
        TunrError::Ipc(format!(
            "timed out waiting for playback daemon response after {}s",
            IPC_TIMEOUT.as_secs()
        ))
    })?
}

/// Send an IPC request to an explicit IPC endpoint without starting a daemon.
pub async fn send_request_to(endpoint: &IpcEndpoint, req: &IpcRequest) -> Result<PlaybackStatus> {
    let stream = IpcStream::connect(endpoint).await?;
    exchange_on_stream(stream, req).await
}

/// Send an IPC request to the configured daemon endpoint without starting a daemon.
pub async fn send_request(paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
    send_request_to(&paths.ipc_endpoint, req).await
}

/// Clean up stale PID/socket files if no live daemon owns `daemon.lock` or the recorded PID.
///
/// Returns `Ok(true)` if stale files were cleaned (or none existed) and spawning is safe,
/// or `Ok(false)` if an existing live daemon lock or PID was found.
pub fn prepare_daemon_spawn(paths: &AppPaths) -> Result<bool> {
    if is_daemon_lock_held(&paths.daemon_lock_path())? {
        return Ok(false);
    }
    if let Some(pid) = read_pid_file(&paths.pid_path)
        && is_pid_alive(pid)
    {
        return Ok(false);
    }

    if let Err(err) = fs::remove_file(&paths.pid_path)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err.into());
    }
    #[cfg(unix)]
    {
        let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
        if let Err(err) = fs::remove_file(socket_path)
            && err.kind() != io::ErrorKind::NotFound
        {
            return Err(err.into());
        }
    }

    Ok(true)
}

async fn spawn_detached_daemon(paths: &AppPaths) -> Result<Option<Child>> {
    let exe = std::env::current_exe().map_err(|err| {
        TunrError::Daemon(format!("could not resolve current executable path: {err}"))
    })?;

    let log_path = paths.log_path.clone();
    let stderr_stdio = tokio::task::spawn_blocking(move || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null())
    })
    .await
    .unwrap_or_else(|_| Stdio::null());

    let mut cmd = Command::new(exe);
    cmd.arg("__daemon")
        .arg("--library-path")
        .arg(&paths.library_path)
        .arg("--config-path")
        .arg(&paths.config_path)
        .arg("--socket-path")
        .arg(paths.ipc_endpoint.to_arg_string())
        .arg("--pid-path")
        .arg(&paths.pid_path)
        .arg("--log-path")
        .arg(&paths.log_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_stdio);

    crate::platform::configure_detached_daemon(&mut cmd);

    let child = cmd.spawn().map_err(|err| {
        TunrError::Daemon(format!("failed to spawn playback daemon: {err}"))
    })?;
    Ok(Some(child))
}

async fn acquire_spawn_lock(paths: &AppPaths) -> Result<paths::FileLock> {
    let spawn_lock_path = paths.daemon_spawn_lock_path();
    let paths_clone = paths.clone();
    tokio::task::spawn_blocking(move || -> Result<paths::FileLock> {
        paths_clone.ensure_dirs()?;
        paths::acquire_file_lock(&spawn_lock_path, true, true)?.ok_or_else(|| {
            TunrError::Daemon("failed to acquire daemon spawn lock".to_string())
        })
    })
    .await
    .map_err(|err| TunrError::Async(format!("daemon spawn lock task failed: {err}")))?
}

async fn ensure_daemon_and_send_with<F, Fut>(
    paths: &AppPaths,
    req: &IpcRequest,
    spawn_daemon: F,
) -> Result<PlaybackStatus>
where
    F: FnOnce(AppPaths) -> Fut,
    Fut: Future<Output = Result<Option<Child>>>,
{
    if let Ok(stream) = IpcStream::connect(&paths.ipc_endpoint).await {
        return exchange_on_stream(stream, req).await;
    }

    let spawn_guard = acquire_spawn_lock(paths).await?;

    // Double-check after acquiring spawn lock in case a concurrent caller just started it.
    if let Ok(stream) = IpcStream::connect(&paths.ipc_endpoint).await {
        drop(spawn_guard);
        return exchange_on_stream(stream, req).await;
    }

    let prep_paths = paths.clone();
    let should_spawn = tokio::task::spawn_blocking(move || prepare_daemon_spawn(&prep_paths))
        .await
        .map_err(|err| TunrError::Async(format!("prepare daemon task failed: {err}")))??;

    if !should_spawn {
        drop(spawn_guard);
        // A live daemon process already exists; give it up to 2s to finish binding its endpoint.
        for _ in 0..DAEMON_SPAWN_MAX_ATTEMPTS {
            if let Ok(stream) = IpcStream::connect(&paths.ipc_endpoint).await {
                return exchange_on_stream(stream, req).await;
            }
            tokio::time::sleep(DAEMON_SPAWN_POLL_INTERVAL).await;
        }
        return Err(TunrError::Daemon(format!(
            "existing daemon did not accept connections at {}",
            paths.ipc_endpoint
        )));
    }

    let mut opt_child = spawn_daemon(paths.clone()).await?;
    let daemon_lock_path = paths.daemon_lock_path();

    for _ in 0..DAEMON_SPAWN_MAX_ATTEMPTS {
        if let Ok(stream) = IpcStream::connect(&paths.ipc_endpoint).await {
            drop(spawn_guard);
            return exchange_on_stream(stream, req).await;
        }
        if let Some(child) = opt_child.as_mut()
            && let Ok(Some(exit_status)) = child.try_wait()
        {
            if let Ok(stream) = IpcStream::connect(&paths.ipc_endpoint).await {
                drop(spawn_guard);
                return exchange_on_stream(stream, req).await;
            }
            if is_daemon_lock_held_async(&daemon_lock_path).await? {
                // Another winning daemon already owns `daemon.lock`; continue polling until it binds.
                opt_child = None;
            } else {
                return Err(TunrError::Daemon(format!(
                    "playback daemon exited prematurely with {exit_status} (see {})",
                    paths.log_path.display()
                )));
            }
        }
        tokio::time::sleep(DAEMON_SPAWN_POLL_INTERVAL).await;
    }

    Err(TunrError::Daemon(format!(
        "timed out waiting for playback daemon endpoint at {}",
        paths.ipc_endpoint
    )))
}

/// Connect to the playback daemon, spawning it detached if not running, serializing
/// cold-start election with `daemon_spawn_lock_path`, cleaning stale files only when
/// `daemon_lock_path` is unheld, and sending `req`.
pub async fn ensure_daemon_and_send(
    paths: &AppPaths,
    req: &IpcRequest,
) -> Result<PlaybackStatus> {
    ensure_daemon_and_send_with(paths, req, |p| async move { spawn_detached_daemon(&p).await })
        .await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::oneshot;

    use super::*;
    use crate::audio::null_sink::NullSink;
    use crate::daemon;

    fn temp_dir(label: &str) -> PathBuf {
        #[cfg(target_os = "macos")]
        let base = PathBuf::from("/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();

        let dir = base.join(format!(
            "tm-ipc-{label}-{}",
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spawn_short_lived_exit(success: bool) -> io::Result<std::process::Child> {
        #[cfg(windows)]
        {
            let arg = if success { "exit 0" } else { "exit 1" };
            StdCommand::new("cmd").args(["/C", arg]).spawn()
        }
        #[cfg(not(windows))]
        {
            let bin = if success { "true" } else { "false" };
            StdCommand::new(bin).spawn()
        }
    }

    #[test]
    fn round_trip_ipc_json_messages() {
        let song = Song {
            id: "s1".to_string(),
            title: "Track One".to_string(),
            artist: Some("Artist".to_string()),
            album: None,
            path: PathBuf::from("/tmp/track1.wav"),
            duration_secs: Some(123.5),
        };
        let req = IpcRequest::Play {
            queue: vec![song.clone()],
            start_index: Some(0),
            mode: PlayMode::Shuffle,
            playlist_name: Some("Mix".to_string()),
            loop_playback: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);

        let playlist_req = IpcRequest::Play {
            queue: vec![song.clone()],
            start_index: None,
            mode: PlayMode::Shuffle,
            playlist_name: Some("Mix".to_string()),
            loop_playback: false,
        };
        let pl_json = serde_json::to_string(&playlist_req).unwrap();
        let pl_decoded: IpcRequest = serde_json::from_str(&pl_json).unwrap();
        assert_eq!(playlist_req, pl_decoded);

        let status = PlaybackStatus {
            state: PlaybackState::Playing,
            mode: PlayMode::Shuffle,
            playlist_name: Some("Mix".to_string()),
            current_track: Some(song.clone()),
            queue_index: 0,
            queue_len: 1,
            position_secs: 12.25,
            duration_secs: Some(123.5),
            queue: vec![song],
            loop_playback: false,
            volume: 75,
        };
        let resp = IpcResponse::Ok(status);
        let resp_json = serde_json::to_string(&resp).unwrap();
        let resp_decoded: IpcResponse = serde_json::from_str(&resp_json).unwrap();
        assert_eq!(resp, resp_decoded);
    }

    #[test]
    fn playback_status_default_sets_default_volume() {
        let status = PlaybackStatus::default();
        assert_eq!(status.state, PlaybackState::Stopped);
        assert_eq!(status.mode, PlayMode::Next);
        assert_eq!(status.playlist_name, None);
        assert_eq!(status.current_track, None);
        assert_eq!(status.queue_index, 0);
        assert_eq!(status.queue_len, 0);
        assert_eq!(status.position_secs, 0.0);
        assert_eq!(status.duration_secs, None);
        assert!(status.queue.is_empty());
        assert!(!status.loop_playback);
        assert_eq!(status.volume, crate::paths::DEFAULT_VOLUME);
    }

    #[test]
    fn set_volume_ipc_request_round_trips() {
        for volume in [0_u8, crate::paths::DEFAULT_VOLUME, crate::paths::MAX_VOLUME] {
            let req = IpcRequest::SetVolume(volume);
            let json = serde_json::to_string(&req).unwrap();
            let decoded: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(req, decoded);
        }
    }

    #[test]
    fn playback_status_missing_volume_json_defaults_to_fifty() {
        let legacy_status_json = r#"{"state":"playing","mode":"next","playlist_name":null,"current_track":null,"queue_index":0,"queue_len":0,"position_secs":0.0,"duration_secs":null,"queue":[],"loop_playback":false}"#;
        let decoded_status: PlaybackStatus = serde_json::from_str(legacy_status_json).unwrap();
        assert_eq!(decoded_status.volume, 50);
        assert_eq!(decoded_status.volume, crate::paths::DEFAULT_VOLUME);

        let explicit_status = PlaybackStatus {
            volume: 85,
            ..PlaybackStatus::default()
        };
        let explicit_json = serde_json::to_string(&explicit_status).unwrap();
        let round_tripped: PlaybackStatus = serde_json::from_str(&explicit_json).unwrap();
        assert_eq!(explicit_status, round_tripped);
    }

    #[test]
    fn loop_playback_serde_defaults_to_false_and_round_trips_true() {
        let legacy_req_json = r#"{"Play":{"queue":[],"start_index":null,"mode":"next","playlist_name":null}}"#;
        let decoded_req: IpcRequest = serde_json::from_str(legacy_req_json).unwrap();
        assert_eq!(
            decoded_req,
            IpcRequest::Play {
                queue: Vec::new(),
                start_index: None,
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            }
        );

        let loop_req = IpcRequest::Play {
            queue: Vec::new(),
            start_index: Some(0),
            mode: PlayMode::Shuffle,
            playlist_name: Some("Mix".to_string()),
            loop_playback: true,
        };
        let loop_req_json = serde_json::to_string(&loop_req).unwrap();
        let round_tripped_req: IpcRequest = serde_json::from_str(&loop_req_json).unwrap();
        assert_eq!(loop_req, round_tripped_req);

        let legacy_status_json = r#"{"state":"playing","mode":"next","playlist_name":null,"current_track":null,"queue_index":0,"queue_len":0,"position_secs":0.0,"duration_secs":null,"queue":[]}"#;
        let decoded_status: PlaybackStatus = serde_json::from_str(legacy_status_json).unwrap();
        assert!(!decoded_status.loop_playback);

        let loop_status = PlaybackStatus {
            state: PlaybackState::Playing,
            loop_playback: true,
            ..PlaybackStatus::default()
        };
        let loop_status_json = serde_json::to_string(&loop_status).unwrap();
        let round_tripped_status: PlaybackStatus = serde_json::from_str(&loop_status_json).unwrap();
        assert_eq!(loop_status, round_tripped_status);
    }

    #[tokio::test]
    async fn send_request_exchanges_status_and_error() {
        let dir = temp_dir("exchange");
        let paths = AppPaths::from_roots(&dir, &dir);
        let mut listener = IpcListener::bind(&paths.ipc_endpoint).unwrap();

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let mut stream = listener.accept().await.unwrap();
                let mut reader = BufReader::new(&mut stream);
                let req: IpcRequest = read_message(&mut reader).await.unwrap().unwrap();
                let resp = match req {
                    IpcRequest::GetStatus => IpcResponse::Ok(PlaybackStatus {
                        state: PlaybackState::Paused,
                        mode: PlayMode::Next,
                        position_secs: 4.5,
                        ..PlaybackStatus::default()
                    }),
                    _ => IpcResponse::Err("nothing queued".to_string()),
                };
                write_message(&mut stream, &resp).await.unwrap();
            }
        });

        let status = send_request(&paths, &IpcRequest::GetStatus).await.unwrap();
        assert_eq!(status.state, PlaybackState::Paused);
        assert_eq!(status.position_secs, 4.5);

        let err = send_request(&paths, &IpcRequest::Next).await.unwrap_err();
        assert!(matches!(err, TunrError::Daemon(msg) if msg == "nothing queued"));

        server.await.unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_message_rejects_oversized_frame() {
        let dir = temp_dir("oversized");
        let paths = AppPaths::from_roots(&dir, &dir);
        let mut listener = IpcListener::bind(&paths.ipc_endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            read_message::<_, IpcRequest>(&mut reader).await
        });

        let mut client = IpcStream::connect(&paths.ipc_endpoint).await.unwrap();
        let huge = vec![b'a'; MAX_IPC_MESSAGE_BYTES + 16];
        let _ = client.write_all(&huge).await;
        let _ = client.write_all(b"\n").await;
        drop(client);

        let err = server.await.unwrap().unwrap_err();
        assert!(
            matches!(err, TunrError::Ipc(ref msg) if msg.contains("limit")),
            "expected size limit error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn send_request_to_missing_socket_reports_ipc_error() {
        let dir = temp_dir("missing");
        #[cfg(unix)]
        let missing_ep = IpcEndpoint::UnixSocket(dir.join("nonexistent.sock"));
        #[cfg(windows)]
        let missing_ep = IpcEndpoint::NamedPipe(format!(
            r"\\.\pipe\tunr-missing-{}",
            uuid::Uuid::new_v4()
        ));

        let err = send_request_to(&missing_ep, &IpcRequest::GetStatus)
            .await
            .unwrap_err();
        assert!(matches!(err, TunrError::Ipc(ref msg) if msg.contains("not running")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_rejects_overlong_unix_socket_path_and_restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("perms");
        let ok_ep = IpcEndpoint::UnixSocket(dir.join("ok.sock"));
        let listener = IpcListener::bind(&ok_ep).unwrap();
        let mode = fs::metadata(dir.join("ok.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);

        let overlong = PathBuf::from("/tmp")
            .join("a".repeat(120))
            .join("tunr.sock");
        let err = match IpcListener::bind(&IpcEndpoint::UnixSocket(overlong)) {
            Ok(_) => panic!("expected overlong socket path to fail"),
            Err(err) => err,
        };
        assert!(matches!(err, TunrError::Ipc(ref msg) if msg.contains("sockaddr_un")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_daemon_spawn_cleans_stale_files_for_dead_pid() {
        let dir = temp_dir("stale");
        let paths = AppPaths::from_roots(&dir, &dir);

        // Spawn and immediately reap a short-lived process so we get an unused dead PID.
        let mut child = spawn_short_lived_exit(true).unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();

        fs::write(&paths.pid_path, format!("{dead_pid}\n")).unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            fs::write(socket_path, "stale").unwrap();
        }

        assert!(!is_pid_alive(dead_pid));
        assert!(prepare_daemon_spawn(&paths).unwrap());
        assert!(!paths.pid_path.exists());
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(!socket_path.exists());
        }

        // Live current process should NOT be cleaned up.
        fs::write(&paths.pid_path, format!("{}\n", std::process::id())).unwrap();
        assert!(is_pid_alive(std::process::id()));
        assert!(!prepare_daemon_spawn(&paths).unwrap());
        assert!(paths.pid_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_cold_start_spawns_only_one_daemon() {
        let dir = temp_dir("cold-start-race");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let daemon_handle = Arc::new(tokio::sync::Mutex::new(None));

        let mut clients = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let p = paths.clone();
            let count = Arc::clone(&spawn_count);
            let slot = Arc::clone(&daemon_handle);
            clients.spawn(async move {
                ensure_daemon_and_send_with(&p, &IpcRequest::GetStatus, |dp| {
                    let count = Arc::clone(&count);
                    let slot = Arc::clone(&slot);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        let (ready_tx, ready_rx) = oneshot::channel();
                        let join = tokio::spawn(async move {
                            daemon::run_daemon_with_sink(
                                &dp,
                                Box::new(NullSink::new()),
                                Some(ready_tx),
                                None,
                            )
                            .await
                        });
                        ready_rx.await.expect("spawned test daemon should bind");
                        *slot.lock().await = Some(join);
                        Ok(None)
                    }
                })
                .await
            });
        }

        while let Some(res) = clients.join_next().await {
            let status = res.unwrap().unwrap();
            assert_eq!(status.state, PlaybackState::Stopped);
        }

        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            1,
            "spawn lock should ensure concurrent cold starters only launch one daemon"
        );

        send_request(&paths, &IpcRequest::Shutdown).await.unwrap();
        if let Some(handle) = daemon_handle.lock().await.take() {
            handle.await.unwrap().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn losing_spawn_child_keeps_polling_when_winner_holds_daemon_lock() {
        let dir = temp_dir("loser-poll");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let winner_handle = Arc::new(tokio::sync::Mutex::new(None));
        let winner_slot = Arc::clone(&winner_handle);

        let status = ensure_daemon_and_send_with(&paths, &IpcRequest::GetStatus, move |dp| {
            let winner_slot = Arc::clone(&winner_slot);
            async move {
                // Hold `daemon.lock` synchronously before returning so it is guaranteed held when
                // the losing short-lived child exits immediately.
                let lock_path = dp.daemon_lock_path();
                let pre_lock = paths::acquire_file_lock(&lock_path, true, true)
                    .unwrap()
                    .expect("test should acquire initial daemon.lock");

                let win_task = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    drop(pre_lock);
                    let (ready_tx, ready_rx) = oneshot::channel();
                    let join = tokio::spawn(async move {
                        daemon::run_daemon_with_sink(
                            &dp,
                            Box::new(NullSink::new()),
                            Some(ready_tx),
                            None,
                        )
                        .await
                    });
                    let _ = ready_rx.await;
                    join
                });

                *winner_slot.lock().await = Some(win_task);
                #[cfg(windows)]
                let loser = Command::new("cmd").args(["/C", "exit 1"]).spawn()?;
                #[cfg(not(windows))]
                let loser = Command::new("false").spawn()?;
                Ok(Some(loser))
            }
        })
        .await
        .unwrap();

        assert_eq!(status.state, PlaybackState::Stopped);

        send_request(&paths, &IpcRequest::Shutdown).await.unwrap();
        if let Some(outer) = winner_handle.lock().await.take() {
            let inner = outer.await.unwrap();
            inner.await.unwrap().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
