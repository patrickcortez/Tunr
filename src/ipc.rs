use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Result, TermusicError};
use crate::library::Song;
use crate::paths::AppPaths;

const IPC_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DAEMON_SPAWN_MAX_ATTEMPTS: usize = 80; // 80 * 25ms = 2.0s

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

/// Write a single newline-delimited JSON message to a writer (e.g. `UnixStream`).
pub fn write_message<T: Serialize>(writer: &mut dyn Write, msg: &T) -> Result<()> {
    let encoded = serde_json::to_string(msg)?;
    writer.write_all(encoded.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Read a single newline-delimited JSON message from a buffered reader (e.g. `BufReader<&UnixStream>`).
/// Returns `Ok(None)` cleanly if the peer closed the stream before sending any bytes.
pub fn read_message<T: DeserializeOwned>(reader: &mut dyn BufRead) -> Result<Option<T>> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    let trimmed = line.trim();
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

fn exchange_on_stream(mut stream: UnixStream, req: &IpcRequest) -> Result<PlaybackStatus> {
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    write_message(&mut stream, req)?;

    let mut reader = BufReader::new(&stream);
    match read_message::<IpcResponse>(&mut reader)? {
        Some(IpcResponse::Ok(status)) => Ok(status),
        Some(IpcResponse::Err(msg)) => Err(TermusicError::Daemon(msg)),
        None => Err(TermusicError::Ipc(
            "playback daemon closed connection without responding".to_string(),
        )),
    }
}

/// Send an IPC request to an explicit Unix socket path without starting a daemon.
pub fn send_request_to(socket_path: &Path, req: &IpcRequest) -> Result<PlaybackStatus> {
    let stream = UnixStream::connect(socket_path).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => TermusicError::Ipc(format!(
            "playback daemon is not running at {}: {err}",
            socket_path.display()
        )),
        _ => TermusicError::Io(err),
    })?;
    exchange_on_stream(stream, req)
}

/// Send an IPC request to the configured daemon socket without starting a daemon.
pub fn send_request(paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
    send_request_to(&paths.socket_path, req)
}

/// Clean up stale PID/socket files if no live daemon owns them.
/// Returns `Ok(true)` if stale files were cleaned (or none existed) and spawning is safe,
/// or `Ok(false)` if an existing live daemon PID was found.
pub fn prepare_daemon_spawn(paths: &AppPaths) -> Result<bool> {
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

/// Connect to the playback daemon, spawning it detached (`setsid`) if not running,
/// cleaning up any stale socket/PID file first, and send `req`.
pub fn ensure_daemon_and_send(paths: &AppPaths, req: &IpcRequest) -> Result<PlaybackStatus> {
    if let Ok(stream) = UnixStream::connect(&paths.socket_path) {
        return exchange_on_stream(stream, req);
    }

    let should_spawn = prepare_daemon_spawn(paths)?;
    if !should_spawn {
        // A live daemon process already exists; give it up to 2s to finish binding its socket.
        for _ in 0..DAEMON_SPAWN_MAX_ATTEMPTS {
            if let Ok(stream) = UnixStream::connect(&paths.socket_path) {
                return exchange_on_stream(stream, req);
            }
            std::thread::sleep(DAEMON_SPAWN_POLL_INTERVAL);
        }
        return Err(TermusicError::Daemon(format!(
            "existing daemon did not accept connections at {}",
            paths.socket_path.display()
        )));
    }

    paths.ensure_dirs()?;

    let exe = std::env::current_exe().map_err(|err| {
        TermusicError::Daemon(format!("could not resolve current executable path: {err}"))
    })?;

    let stderr_stdio = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_path)
        .map(Stdio::from)
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

    let mut child = cmd.spawn().map_err(|err| {
        TermusicError::Daemon(format!("failed to spawn playback daemon: {err}"))
    })?;

    for _ in 0..DAEMON_SPAWN_MAX_ATTEMPTS {
        if let Ok(stream) = UnixStream::connect(&paths.socket_path) {
            return exchange_on_stream(stream, req);
        }
        if let Ok(Some(exit_status)) = child.try_wait() {
            if let Ok(stream) = UnixStream::connect(&paths.socket_path) {
                return exchange_on_stream(stream, req);
            }
            return Err(TermusicError::Daemon(format!(
                "playback daemon exited prematurely with {exit_status} (see {})",
                paths.log_path.display()
            )));
        }
        std::thread::sleep(DAEMON_SPAWN_POLL_INTERVAL);
    }

    Err(TermusicError::Daemon(format!(
        "timed out waiting for playback daemon socket at {}",
        paths.socket_path.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    use super::*;

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

    #[test]
    fn send_request_exchanges_status_and_error() {
        let dir = temp_dir("exchange");
        let paths = AppPaths::from_roots(&dir, &dir);
        let listener = UnixListener::bind(&paths.socket_path).unwrap();

        let server = thread::spawn(move || {
            for conn in listener.incoming().take(2) {
                let mut stream = conn.unwrap();
                let mut reader = BufReader::new(&stream);
                let req: IpcRequest = read_message(&mut reader).unwrap().unwrap();
                let resp = match req {
                    IpcRequest::GetStatus => IpcResponse::Ok(PlaybackStatus {
                        state: PlaybackState::Paused,
                        mode: PlayMode::Next,
                        position_secs: 4.5,
                        ..PlaybackStatus::default()
                    }),
                    _ => IpcResponse::Err("nothing queued".to_string()),
                };
                write_message(&mut stream, &resp).unwrap();
            }
        });

        let status = send_request(&paths, &IpcRequest::GetStatus).unwrap();
        assert_eq!(status.state, PlaybackState::Paused);
        assert_eq!(status.position_secs, 4.5);

        let err = send_request(&paths, &IpcRequest::Next).unwrap_err();
        assert!(matches!(err, TermusicError::Daemon(msg) if msg == "nothing queued"));

        server.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn send_request_to_missing_socket_reports_ipc_error() {
        let dir = temp_dir("missing");
        let socket_path = dir.join("nonexistent.sock");
        let err = send_request_to(&socket_path, &IpcRequest::GetStatus).unwrap_err();
        assert!(matches!(err, TermusicError::Ipc(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_daemon_spawn_cleans_stale_files_for_dead_pid() {
        let dir = temp_dir("stale");
        let paths = AppPaths::from_roots(&dir, &dir);

        // Spawn and immediately reap a short-lived process so we get an unused dead PID.
        let mut child = Command::new("true").spawn().unwrap();
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
}
