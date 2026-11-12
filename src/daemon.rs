use std::fs;
use std::io::{self, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::audio::AudioSink;
use crate::audio::pulse::PulseSimpleSink;
use crate::error::{Result, TermusicError};
use crate::ipc::{self, IpcRequest, IpcResponse};
use crate::paths::AppPaths;
use crate::player::{self, EngineCommand, EngineHandle};

const ACCEPT_POLL_TIMEOUT_MS: i32 = 100;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);

static SIGNAL_SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_termination_signal(_sig: libc::c_int) {
    SIGNAL_SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() -> Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_termination_signal as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;

        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}

struct CleanupGuard {
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.pid_path);
    }
}

/// Start the background playback daemon using PulseAudio (`PulseSimpleSink`) and `SIGINT`/`SIGTERM`
/// shutdown handlers.
pub fn run_daemon(paths: &AppPaths) -> Result<()> {
    paths.ensure_dirs()?;
    prepare_daemon_files(paths)?;

    let sink = PulseSimpleSink::new()?;
    SIGNAL_SHUTDOWN.store(false, Ordering::SeqCst);
    install_signal_handlers()?;

    run_daemon_impl(paths, Box::new(sink), None, true)
}

/// Start the daemon IPC server and audio engine using a caller-supplied [`AudioSink`].
///
/// If `ready_tx` is provided, signals `()` once the PID file is written and the `UnixListener` is
/// bound and ready to accept connections.
pub fn run_daemon_with_sink(
    paths: &AppPaths,
    sink: Box<dyn AudioSink>,
    ready_tx: Option<mpsc::Sender<()>>,
) -> Result<()> {
    run_daemon_impl(paths, sink, ready_tx, false)
}

fn run_daemon_impl(
    paths: &AppPaths,
    sink: Box<dyn AudioSink>,
    ready_tx: Option<mpsc::Sender<()>>,
    watch_process_signals: bool,
) -> Result<()> {
    paths.ensure_dirs()?;
    prepare_daemon_files(paths)?;

    fs::write(&paths.pid_path, format!("{}\n", std::process::id()))?;

    let listener = match UnixListener::bind(&paths.socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            let _ = fs::remove_file(&paths.pid_path);
            return Err(err.into());
        }
    };
    let _cleanup = CleanupGuard {
        socket_path: paths.socket_path.clone(),
        pid_path: paths.pid_path.clone(),
    };
    listener.set_nonblocking(true)?;

    let (engine, engine_thread) = player::spawn_engine(sink)?;

    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    let local_shutdown = AtomicBool::new(false);
    let loop_result = run_accept_loop(
        &listener,
        &engine,
        &local_shutdown,
        watch_process_signals,
    );

    if !local_shutdown.load(Ordering::SeqCst) {
        let _ = engine.send(EngineCommand::Shutdown);
    }
    let _ = engine_thread.join();

    loop_result
}

fn prepare_daemon_files(paths: &AppPaths) -> Result<()> {
    if paths.socket_path.exists() {
        if read_live_pid(&paths.pid_path).is_some() {
            return Err(TermusicError::Daemon("daemon already running".to_string()));
        }
        remove_if_exists(&paths.socket_path)?;
        remove_if_exists(&paths.pid_path)?;
    } else if paths.pid_path.exists() && read_live_pid(&paths.pid_path).is_none() {
        remove_if_exists(&paths.pid_path)?;
    }
    Ok(())
}

fn read_live_pid(pid_path: &Path) -> Option<i32> {
    let raw = fs::read_to_string(pid_path).ok()?;
    let pid = raw.trim().parse::<i32>().ok()?;
    ipc::is_pid_alive(pid).then_some(pid)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn run_accept_loop(
    listener: &UnixListener,
    engine: &EngineHandle,
    local_shutdown: &AtomicBool,
    watch_process_signals: bool,
) -> Result<()> {
    loop {
        if should_stop(local_shutdown, watch_process_signals) {
            break;
        }

        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ACCEPT_POLL_TIMEOUT_MS) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if rc == 0 {
            continue;
        }

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(err) = handle_client(stream, engine, local_shutdown) {
                        eprintln!("daemon client error: {err}");
                    }
                    if local_shutdown.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    if should_stop(local_shutdown, watch_process_signals) {
                        return Ok(());
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    Ok(())
}

fn should_stop(local_shutdown: &AtomicBool, watch_process_signals: bool) -> bool {
    local_shutdown.load(Ordering::SeqCst)
        || (watch_process_signals && SIGNAL_SHUTDOWN.load(Ordering::SeqCst))
}

fn handle_client(
    mut stream: UnixStream,
    engine: &EngineHandle,
    local_shutdown: &AtomicBool,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;

    let req = {
        let mut reader = BufReader::new(&stream);
        match ipc::read_message::<IpcRequest>(&mut reader) {
            Ok(Some(req)) => req,
            Ok(None) => return Ok(()),
            Err(err) => {
                let _ = ipc::write_message(&mut stream, &IpcResponse::Err(err.to_string()));
                return Ok(());
            }
        }
    };

    let response = match req {
        IpcRequest::GetStatus => IpcResponse::Ok(engine.status()),
        IpcRequest::Shutdown => {
            local_shutdown.store(true, Ordering::SeqCst);
            match engine.send(EngineCommand::Shutdown) {
                Ok(status) => IpcResponse::Ok(status),
                Err(err) => IpcResponse::Err(err.to_string()),
            }
        }
        other => {
            let cmd = map_ipc_to_engine_command(other);
            match engine.send(cmd) {
                Ok(status) => IpcResponse::Ok(status),
                Err(err) => IpcResponse::Err(err.to_string()),
            }
        }
    };

    if let Err(err) = ipc::write_message(&mut stream, &response)
        && !is_client_disconnect(&err)
    {
        return Err(err);
    }

    Ok(())
}

fn map_ipc_to_engine_command(req: IpcRequest) -> EngineCommand {
    match req {
        IpcRequest::Play {
            queue,
            start_index,
            mode,
            playlist_name,
        } => EngineCommand::LoadQueue {
            queue,
            start_index,
            mode,
            playlist_name,
        },
        IpcRequest::Next => EngineCommand::Next,
        IpcRequest::Prev => EngineCommand::Prev,
        IpcRequest::SeekRelative(delta) => EngineCommand::SeekRelative(delta),
        IpcRequest::SeekAbsolute(target) => EngineCommand::SeekAbsolute(target),
        IpcRequest::TogglePause => EngineCommand::TogglePause,
        IpcRequest::SetPaused(paused) => EngineCommand::SetPaused(paused),
        IpcRequest::Stop => EngineCommand::Stop,
        IpcRequest::GetStatus => EngineCommand::GetStatus,
        IpcRequest::Shutdown => EngineCommand::Shutdown,
    }
}

fn is_client_disconnect(err: &TermusicError) -> bool {
    matches!(
        err,
        TermusicError::Io(io_err)
            if matches!(
                io_err.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            )
    )
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;
    use std::process::Command;
    use std::thread;

    use super::*;
    use crate::audio::null_sink::NullSink;
    use crate::ipc::{PlayMode, PlaybackState};
    use crate::library::Song;

    fn temp_workspace(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-daemon-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_wav(path: &Path, sample_rate: u32, duration_secs: f64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let channels: u16 = 1;
        let bits_per_sample: u16 = 16;
        let num_frames = (sample_rate as f64 * duration_secs).round() as u32;
        let block_align = channels * (bits_per_sample / 8);
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = num_frames * u32::from(block_align);
        let riff_chunk_size = 36 + data_size;

        let mut buf = Vec::with_capacity((44 + data_size) as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());

        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = ((2.0 * PI * 440.0 * t).sin() * 0.25 * i16::MAX as f32) as i16;
            buf.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(path, buf).unwrap();
    }

    fn make_song(dir: &Path, title: &str, duration_secs: f64) -> Song {
        let path = dir.join(format!("{title}.wav"));
        write_wav(&path, 44_100, duration_secs);
        Song {
            id: format!("id-{title}"),
            title: title.to_string(),
            artist: Some("Daemon Tester".to_string()),
            album: None,
            path,
            duration_secs: Some(duration_secs),
        }
    }

    #[test]
    fn daemon_ipc_round_trips_playback_controls_and_cleans_up_on_shutdown() {
        let dir = temp_workspace("roundtrip");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let songs = vec![
            make_song(&dir, "Alpha", 2.0),
            make_song(&dir, "Beta", 2.0),
        ];

        let sink = NullSink::with_pacing(Duration::from_millis(8));
        let stats = sink.stats();
        let (ready_tx, ready_rx) = mpsc::channel();
        let daemon_paths = paths.clone();

        let daemon_thread = thread::spawn(move || {
            run_daemon_with_sink(&daemon_paths, Box::new(sink), Some(ready_tx))
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("daemon should signal readiness");

        assert!(paths.socket_path.exists());
        assert!(paths.pid_path.exists());
        let recorded_pid: u32 = fs::read_to_string(&paths.pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(recorded_pid, std::process::id());

        // Initial GetStatus is Stopped.
        let initial = ipc::send_request(&paths, &IpcRequest::GetStatus).unwrap();
        assert_eq!(initial.state, PlaybackState::Stopped);
        assert!(initial.current_track.is_none());

        // Next before Play returns clean Daemon error over IPC.
        let err = ipc::send_request(&paths, &IpcRequest::Next).unwrap_err();
        assert!(matches!(err, TermusicError::Daemon(_)));

        // Play starts queue at Alpha.
        let playing = ipc::send_request(
            &paths,
            &IpcRequest::Play {
                queue: songs,
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: Some("DaemonMix".to_string()),
            },
        )
        .unwrap();
        assert_eq!(playing.state, PlaybackState::Playing);
        assert_eq!(playing.playlist_name.as_deref(), Some("DaemonMix"));
        assert_eq!(playing.current_track.as_ref().unwrap().title, "Alpha");
        assert_eq!(playing.queue_index, 0);
        assert_eq!(playing.queue_len, 2);

        // GetStatus reports active playback.
        let polled = ipc::send_request(&paths, &IpcRequest::GetStatus).unwrap();
        assert_eq!(polled.state, PlaybackState::Playing);
        assert_eq!(polled.current_track.as_ref().unwrap().title, "Alpha");

        // Next advances to Beta; Prev returns to Alpha.
        let next = ipc::send_request(&paths, &IpcRequest::Next).unwrap();
        assert_eq!(next.current_track.as_ref().unwrap().title, "Beta");
        assert_eq!(next.queue_index, 1);

        let prev = ipc::send_request(&paths, &IpcRequest::Prev).unwrap();
        assert_eq!(prev.current_track.as_ref().unwrap().title, "Alpha");
        assert_eq!(prev.queue_index, 0);
        assert_eq!(prev.position_secs, 0.0);

        // SeekRelative forward then rewind past 0.
        let adv = ipc::send_request(&paths, &IpcRequest::SeekRelative(0.5)).unwrap();
        assert!(adv.position_secs >= 0.4);
        let rew = ipc::send_request(&paths, &IpcRequest::SeekRelative(-5.0)).unwrap();
        assert_eq!(rew.position_secs, 0.0);

        // TogglePause -> Paused -> TogglePause -> Playing.
        let paused = ipc::send_request(&paths, &IpcRequest::TogglePause).unwrap();
        assert_eq!(paused.state, PlaybackState::Paused);
        let resumed = ipc::send_request(&paths, &IpcRequest::TogglePause).unwrap();
        assert_eq!(resumed.state, PlaybackState::Playing);

        // Stop -> Stopped.
        let stopped = ipc::send_request(&paths, &IpcRequest::Stop).unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert_eq!(stopped.position_secs, 0.0);
        assert!(stopped.current_track.is_none());

        // Shutdown stops daemon and removes socket/pid files.
        let bye = ipc::send_request(&paths, &IpcRequest::Shutdown).unwrap();
        assert_eq!(bye.state, PlaybackState::Stopped);

        daemon_thread.join().unwrap().unwrap();
        assert!(!paths.socket_path.exists());
        assert!(!paths.pid_path.exists());
        assert!(!stats.is_open());
        assert!(stats.flush_count() >= 1);
        assert!(stats.close_count() >= 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_rejects_second_instance_when_live_pid_owns_socket() {
        let dir = temp_workspace("duplicate");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));

        let (ready_tx, ready_rx) = mpsc::channel();
        let p1 = paths.clone();
        let t1 = thread::spawn(move || {
            run_daemon_with_sink(&p1, Box::new(NullSink::new()), Some(ready_tx))
        });

        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let dup_err = run_daemon_with_sink(&paths, Box::new(NullSink::new()), None).unwrap_err();
        assert!(
            matches!(dup_err, TermusicError::Daemon(ref msg) if msg.contains("already running")),
            "unexpected duplicate daemon error: {dup_err:?}"
        );

        // First daemon's socket and PID remain intact and functional.
        assert!(paths.socket_path.exists());
        assert!(paths.pid_path.exists());
        let status = ipc::send_request(&paths, &IpcRequest::GetStatus).unwrap();
        assert_eq!(status.state, PlaybackState::Stopped);

        ipc::send_request(&paths, &IpcRequest::Shutdown).unwrap();
        t1.join().unwrap().unwrap();
        assert!(!paths.socket_path.exists());
        assert!(!paths.pid_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_cleans_stale_socket_and_pid_for_dead_process() {
        let dir = temp_workspace("stale-startup");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        paths.ensure_dirs().unwrap();

        let mut child = Command::new("true").spawn().unwrap();
        let dead_pid = child.id() as i32;
        child.wait().unwrap();
        assert!(!ipc::is_pid_alive(dead_pid));

        fs::write(&paths.pid_path, format!("{dead_pid}\n")).unwrap();
        fs::write(&paths.socket_path, "stale-socket").unwrap();

        let (ready_tx, ready_rx) = mpsc::channel();
        let p = paths.clone();
        let daemon_thread = thread::spawn(move || {
            run_daemon_with_sink(&p, Box::new(NullSink::new()), Some(ready_tx))
        });

        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let status = ipc::send_request(&paths, &IpcRequest::GetStatus).unwrap();
        assert_eq!(status.state, PlaybackState::Stopped);

        ipc::send_request(&paths, &IpcRequest::Shutdown).unwrap();
        daemon_thread.join().unwrap().unwrap();
        assert!(!paths.socket_path.exists());
        assert!(!paths.pid_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}