use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::error::{Result, TermusicError};
use crate::library::Song;
use crate::paths::{self, AppPaths};

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

/// Requests sent from CLI/TUI clients to the Termusic playback daemon over Unix socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcRequest {
    Play {
        queue: Vec<Song>,
        start_index: Option<usize>,
        mode: PlayMode,
        playlist_name: Option<String>,
    },
    Next,
    Prev,
    SeekRelative(f64),
    SeekAbsolute(f64),
    TogglePause,
    SetPaused(bool),
    Stop,
    GetStatus,
    Shutdown,
}

/// Snapshot of current daemon playback state returned in `IpcResponse::Ok`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
}

/// Response returned by the Termusic playback daemon for an `IpcRequest`.
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
        return Err(TermusicError::Ipc(format!(
            "IPC message exceeded {MAX_IPC_MESSAGE_BYTES} byte limit"
        )));
    }

    let text = std::str::from_utf8(&buf)
        .map_err(|err| TermusicError::Ipc(format!("IPC message is not valid UTF-8: {err}")))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(TermusicError::Ipc(
            "received empty IPC message frame".to_string(),
        ));
    }
    let parsed = serde_json::from_str::<T>(trimmed)?;
    Ok(Some(parsed))
}

/// Check whether a process with `pid` currently exists according to `kill(pid, 0)`.
pub fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

fn read_pid_file(pid_path: &Path) -> Option<i32> {
    let contents = fs::read_to_string(pid_path).ok()?;
    contents.trim().parse::<i32>().ok()
}

fn is_daemon_lock_held(lock_path: &Path) -> Result<bool> {
    Ok(paths::acquire_file_lock(lock_path, true, false)?.is_none())
}

async fn is_daemon_lock_held_async(lock_path: &Path) -> Result<bool> {
    let lock_path = lock_path.to_path_buf();
    tokio::task::spawn_blocking(move || is_daemon_lock_held(&lock_path))
        .await
        .map_err(|err| TermusicError::Async(format!("daemon lock check task failed: {err}")))?
}

async fn exchange_on_stream(mut stream: UnixStream, req: &IpcRequest) -> Result<PlaybackStatus> {
    tokio::time::timeout(IPC_TIMEOUT, async {
        write_message(&mut stream, req).await?;

        let mut reader = BufReader::new(&mut stream);
        match read_message::<_, IpcResponse>(&mut reader).await? {
            Some(IpcResponse::Ok(status)) => Ok(status),
            Some(IpcResponse::Err(msg)) => Err(TermusicError::Daemon(msg)),
            None => Err(TermusicError::Ipc(
                "playback daemon closed connection without responding".to_string(),
            )),
        }
    })
    .await
    .map_err(|_| {
        TermusicError::Ipc(format!(
            "timed out waiting for playback daemon response after {}s",
            IPC_TIMEOUT.as_secs()
        ))
    })?
}

/// Send an IPC request to an explicit Unix socket path without starting a daemon.
pub async fn send_request_to(socket_path: &Path, req: &IpcRequest) -> Result<PlaybackStatus> {
    let stream = UnixStream::connect(socket_path).await.map_err(|err| match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => TermusicError::Ipc(format!(
            "playback daemon is not running at {}: {err}",
            socket_path.display()
        )),
        _ => TermusicError::Io(err),
    })?;
    exchange_on_stream(stream, req).await
}

/// Send an IPC request to the configured daemon socket without starting a daemon.
pub async fn send_request(paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
    send_request_to(&paths.socket_path, req).await
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
    if let Err(err) = fs::remove_file(&paths.socket_path)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err.into());
    }

    Ok(true)
}

async fn spawn_detached_daemon(paths: &AppPaths) -> Result<Option<Child>> {
    let exe = std::env::current_exe().map_err(|err| {
        TermusicError::Daemon(format!("could not resolve current executable path: {err}"))
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
        .arg("--socket-path")
        .arg(&paths.socket_path)
        .arg("--pid-path")
        .arg(&paths.pid_path)
        .arg("--log-path")
        .arg(&paths.log_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_stdio);

    // SAFETY: `setsid` is async-signal-safe and detaches the spawned daemon into a new session.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().map_err(|err| {
        TermusicError::Daemon(format!("failed to spawn playback daemon: {err}"))
    })?;
    Ok(Some(child))
}

async fn acquire_spawn_lock(paths: &AppPaths) -> Result<File> {
    let spawn_lock_path = paths.daemon_spawn_lock_path();
    let paths_clone = paths.clone();
    tokio::task::spawn_blocking(move || -> Result<File> {
        paths_clone.ensure_dirs()?;
        paths::acquire_file_lock(&spawn_lock_path, true, true)?.ok_or_else(|| {
            TermusicError::Daemon("failed to acquire daemon spawn lock".to_string())
        })
    })
    .await
    .map_err(|err| TermusicError::Async(format!("daemon spawn lock task failed: {err}")))?
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
    if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
        return exchange_on_stream(stream, req).await;
    }

    let spawn_guard = acquire_spawn_lock(paths).await?;

    // Double-check after acquiring spawn lock in case a concurrent caller just started it.
    if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
        drop(spawn_guard);
        return exchange_on_stream(stream, req).await;
    }

    let prep_paths = paths.clone();
    let should_spawn = tokio::task::spawn_blocking(move || prepare_daemon_spawn(&prep_paths))
        .await
        .map_err(|err| TermusicError::Async(format!("prepare daemon task failed: {err}")))??;

    if !should_spawn {
        drop(spawn_guard);
        // A live daemon process already exists; give it up to 2s to finish binding its socket.
        for _ in 0..DAEMON_SPAWN_MAX_ATTEMPTS {
            if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
                return exchange_on_stream(stream, req).await;
            }
            tokio::time::sleep(DAEMON_SPAWN_POLL_INTERVAL).await;
        }
        return Err(TermusicError::Daemon(format!(
            "existing daemon did not accept connections at {}",
            paths.socket_path.display()
        )));
    }

    let mut opt_child = spawn_daemon(paths.clone()).await?;
    let daemon_lock_path = paths.daemon_lock_path();

    for _ in 0..DAEMON_SPAWN_MAX_ATTEMPTS {
        if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
            drop(spawn_guard);
            return exchange_on_stream(stream, req).await;
        }
        if let Some(child) = opt_child.as_mut()
            && let Ok(Some(exit_status)) = child.try_wait()
        {
            if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
                drop(spawn_guard);
                return exchange_on_stream(stream, req).await;
            }
            if is_daemon_lock_held_async(&daemon_lock_path).await? {
                // Another winning daemon already owns `daemon.lock`; continue polling until it binds.
                opt_child = None;
            } else {
                return Err(TermusicError::Daemon(format!(
                    "playback daemon exited prematurely with {exit_status} (see {})",
                    paths.log_path.display()
                )));
            }
        }
        tokio::time::sleep(DAEMON_SPAWN_POLL_INTERVAL).await;
    }

    Err(TermusicError::Daemon(format!(
        "timed out waiting for playback daemon socket at {}",
        paths.socket_path.display()
    )))
}

/// Connect to the playback daemon, spawning it detached (`setsid`) if not running,
/// serializing cold-start election with `daemon_spawn_lock_path`, cleaning stale files only when
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

    use tokio::net::UnixListener;
    use tokio::sync::oneshot;

    use super::*;
    use crate::audio::null_sink::NullSink;
    use crate::daemon;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-ipc-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
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
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);

        let playlist_req = IpcRequest::Play {
            queue: vec![song.clone()],
            start_index: None,
            mode: PlayMode::Shuffle,
            playlist_name: Some("Mix".to_string()),
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
        };
        let resp = IpcResponse::Ok(status);
        let resp_json = serde_json::to_string(&resp).unwrap();
        let resp_decoded: IpcResponse = serde_json::from_str(&resp_json).unwrap();
        assert_eq!(resp, resp_decoded);
    }

    #[tokio::test]
    async fn send_request_exchanges_status_and_error() {
        let dir = temp_dir("exchange");
        let paths = AppPaths::from_roots(&dir, &dir);
        let listener = UnixListener::bind(&paths.socket_path).unwrap();

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
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
        assert!(matches!(err, TermusicError::Daemon(msg) if msg == "nothing queued"));

        server.await.unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_message_rejects_oversized_frame() {
        let dir = temp_dir("oversized");
        let socket_path = dir.join("oversized.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            read_message::<_, IpcRequest>(&mut reader).await
        });

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        let huge = vec![b'a'; MAX_IPC_MESSAGE_BYTES + 16];
        let _ = client.write_all(&huge).await;
        let _ = client.write_all(b"\n").await;
        drop(client);

        let err = server.await.unwrap().unwrap_err();
        assert!(
            matches!(err, TermusicError::Ipc(ref msg) if msg.contains("limit")),
            "expected size limit error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn send_request_to_missing_socket_reports_ipc_error() {
        let dir = temp_dir("missing");
        let socket_path = dir.join("nonexistent.sock");
        let err = send_request_to(&socket_path, &IpcRequest::GetStatus)
            .await
            .unwrap_err();
        assert!(matches!(err, TermusicError::Ipc(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_daemon_spawn_cleans_stale_files_for_dead_pid() {
        let dir = temp_dir("stale");
        let paths = AppPaths::from_roots(&dir, &dir);

        // Spawn and immediately reap a short-lived process so we get an unused dead PID.
        let mut child = StdCommand::new("true").spawn().unwrap();
        let dead_pid = child.id() as i32;
        child.wait().unwrap();

        fs::write(&paths.pid_path, format!("{dead_pid}\n")).unwrap();
        fs::write(&paths.socket_path, "stale").unwrap();

        assert!(!is_pid_alive(dead_pid));
        assert!(prepare_daemon_spawn(&paths).unwrap());
        assert!(!paths.pid_path.exists());
        assert!(!paths.socket_path.exists());

        // Live current process should NOT be cleaned up.
        fs::write(&paths.pid_path, format!("{}\n", std::process::id())).unwrap();
        assert!(is_pid_alive(std::process::id() as i32));
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
                // the losing `false` child exits immediately.
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
