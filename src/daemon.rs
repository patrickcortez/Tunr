use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;

use crate::audio::{self, AudioSink};
use crate::error::{Result, TunrError};
use crate::ipc::{self, IpcListener, IpcRequest, IpcResponse, IpcStream};
use crate::paths::{self, AppPaths, IpcEndpoint};
use crate::platform::ShutdownSignals;
use crate::player::{self, EngineCommand, EngineHandle};

const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const ENGINE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

struct CleanupGuard {
    ipc_endpoint: IpcEndpoint,
    pid_path: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        match &self.ipc_endpoint {
            #[cfg(unix)]
            IpcEndpoint::UnixSocket(socket_path) => {
                let _ = fs::remove_file(socket_path);
            }
            #[cfg(windows)]
            IpcEndpoint::NamedPipe(_) => {}
        }
        let _ = fs::remove_file(&self.pid_path);
    }
}

/// Start the background playback daemon using the default OS audio backend and cross-platform
/// shutdown signal handlers.
pub async fn run_daemon(paths: &AppPaths) -> Result<()> {
    run_daemon_impl(paths, audio::default_sink, None, None, true).await
}

/// Start the daemon IPC server and audio engine using a caller-supplied [`AudioSink`].
///
/// If `ready_tx` is provided, signals `()` once `daemon.lock` is held, the PID file is written,
/// and the `IpcListener` is bound and ready to accept connections. Optional `shutdown_rx` lets
/// callers trigger graceful shutdown without modifying process-wide signal handlers.
pub async fn run_daemon_with_sink(
    paths: &AppPaths,
    sink: Box<dyn AudioSink>,
    ready_tx: Option<oneshot::Sender<()>>,
    shutdown_rx: Option<watch::Receiver<bool>>,
) -> Result<()> {
    run_daemon_impl(paths, move || Ok(sink), ready_tx, shutdown_rx, false).await
}

async fn run_daemon_impl<F>(
    paths: &AppPaths,
    make_sink: F,
    ready_tx: Option<oneshot::Sender<()>>,
    ext_shutdown_rx: Option<watch::Receiver<bool>>,
    watch_process_signals: bool,
) -> Result<()>
where
    F: FnOnce() -> Result<Box<dyn AudioSink>> + Send + 'static,
{
    let mut proc_signals = if watch_process_signals {
        Some(ShutdownSignals::new()?)
    } else {
        None
    };

    let startup_paths = paths.clone();
    let (_daemon_lock, sink, initial_volume): (paths::FileLock, Box<dyn AudioSink>, u8) =
        tokio::task::spawn_blocking(move || {
        startup_paths.ensure_dirs()?;

        // Acquire exclusive ownership of `daemon.lock` BEFORE inspecting or deleting any socket/PID
        // files so concurrent cold-starting daemons never clobber the active owner's files.
        let Some(daemon_lock) =
            paths::acquire_file_lock(&startup_paths.daemon_lock_path(), true, false)?
        else {
            return Err(TunrError::Daemon("daemon already running".to_string()));
        };

        prepare_daemon_files(&startup_paths)?;
        let initial_volume = crate::paths::load_workspace_config(&startup_paths)
            .ok()
            .map(|cfg| crate::paths::effective_volume(&cfg))
            .unwrap_or(crate::paths::DEFAULT_VOLUME);
        let sink = make_sink()?;
        fs::write(
            &startup_paths.pid_path,
            format!("{}\n", std::process::id()),
        )?;
        Ok((daemon_lock, sink, initial_volume))
    })
    .await
    .map_err(|err| TunrError::Async(format!("daemon startup task failed: {err}")))??;

    let listener = match IpcListener::bind(&paths.ipc_endpoint) {
        Ok(listener) => listener,
        Err(err) => {
            let _ = fs::remove_file(&paths.pid_path);
            return Err(err);
        }
    };
    let _cleanup = CleanupGuard {
        ipc_endpoint: paths.ipc_endpoint.clone(),
        pid_path: paths.pid_path.clone(),
    };

    let (engine, engine_thread) = player::spawn_engine_with_volume(sink, initial_volume)?;

    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    let (shutdown_tx, shutdown_rx_local) = watch::channel(false);
    let mut client_tasks: JoinSet<()> = JoinSet::new();

    let loop_result = run_accept_loop(
        listener,
        &engine,
        &shutdown_tx,
        shutdown_rx_local,
        ext_shutdown_rx,
        &mut proc_signals,
        &mut client_tasks,
    )
    .await;

    drain_client_tasks(&mut client_tasks).await;

    if !*shutdown_tx.borrow() {
        let _ = tokio::time::timeout(
            ENGINE_SHUTDOWN_TIMEOUT,
            engine.send(EngineCommand::Shutdown),
        )
        .await;
    }
    drop(engine);

    let join_res = tokio::task::spawn_blocking(move || engine_thread.join())
        .await
        .map_err(|err| TunrError::Async(format!("engine join task failed: {err}")))
        .and_then(|res| {
            res.map_err(|_| TunrError::Async("audio engine thread panicked".to_string()))
        });

    // Drop socket/PID cleanup guard BEFORE releasing `_daemon_lock` so no racing startup removes
    // our files or binds out of order.
    drop(_cleanup);
    drop(_daemon_lock);

    loop_result?;
    join_res?;
    Ok(())
}

fn prepare_daemon_files(paths: &AppPaths) -> Result<()> {
    #[cfg(unix)]
    {
        let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
        remove_if_exists(socket_path)?;
    }
    remove_if_exists(&paths.pid_path)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

async fn wait_watch_true(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
    std::future::pending::<()>().await
}

async fn wait_optional_watch_true(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => wait_watch_true(rx).await,
        None => std::future::pending::<()>().await,
    }
}

async fn wait_optional_signals(signals: &mut Option<ShutdownSignals>) {
    match signals {
        Some(s) => {
            let _ = s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

async fn run_accept_loop(
    mut listener: IpcListener,
    engine: &EngineHandle,
    shutdown_tx: &watch::Sender<bool>,
    mut local_shutdown_rx: watch::Receiver<bool>,
    mut ext_shutdown_rx: Option<watch::Receiver<bool>>,
    proc_signals: &mut Option<ShutdownSignals>,
    client_tasks: &mut JoinSet<()>,
) -> Result<()> {
    loop {
        while client_tasks.try_join_next().is_some() {}

        tokio::select! {
            biased;
            _ = wait_watch_true(&mut local_shutdown_rx) => {
                break;
            }
            _ = wait_optional_watch_true(&mut ext_shutdown_rx) => {
                break;
            }
            _ = wait_optional_signals(proc_signals) => {
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok(stream) => {
                        let engine_clone = engine.clone();
                        let shutdown_tx_clone = shutdown_tx.clone();
                        client_tasks.spawn(async move {
                            if let Err(err) =
                                handle_client(stream, engine_clone, shutdown_tx_clone).await
                            {
                                eprintln!("daemon client error: {err}");
                            }
                        });
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }
    }

    drop(listener);
    Ok(())
}

async fn drain_client_tasks(client_tasks: &mut JoinSet<()>) {
    if client_tasks.is_empty() {
        return;
    }
    let deadline = tokio::time::sleep(CLIENT_SHUTDOWN_GRACE);
    tokio::pin!(deadline);

    while !client_tasks.is_empty() {
        tokio::select! {
            joined = client_tasks.join_next() => {
                if joined.is_none() {
                    break;
                }
            }
            _ = &mut deadline => {
                client_tasks.abort_all();
                while client_tasks.join_next().await.is_some() {}
                break;
            }
        }
    }
}

async fn handle_client(
    mut stream: IpcStream,
    engine: EngineHandle,
    shutdown_tx: watch::Sender<bool>,
) -> Result<()> {
    let req = {
        let mut reader = BufReader::new(&mut stream);
        match tokio::time::timeout(
            CLIENT_IO_TIMEOUT,
            ipc::read_message::<_, IpcRequest>(&mut reader),
        )
        .await
        {
            Ok(Ok(Some(req))) => req,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(err)) => {
                let _ = tokio::time::timeout(
                    CLIENT_IO_TIMEOUT,
                    ipc::write_message(&mut stream, &IpcResponse::Err(err.to_string())),
                )
                .await;
                return Ok(());
            }
            Err(_) => {
                return Err(TunrError::Ipc(
                    "timed out reading IPC request from client".to_string(),
                ));
            }
        }
    };

    let is_shutdown = matches!(req, IpcRequest::Shutdown);
    let response = match req {
        IpcRequest::GetStatus => IpcResponse::Ok((*engine.status()).clone()),
        IpcRequest::Shutdown => {
            match tokio::time::timeout(
                CLIENT_IO_TIMEOUT,
                engine.send(EngineCommand::Shutdown),
            )
            .await
            {
                Ok(Ok(status)) => IpcResponse::Ok(status),
                Ok(Err(err)) => IpcResponse::Err(err.to_string()),
                Err(_) => {
                    IpcResponse::Err("timed out waiting for audio engine shutdown".to_string())
                }
            }
        }
        other => {
            let cmd = map_ipc_to_engine_command(other);
            match tokio::time::timeout(CLIENT_IO_TIMEOUT, engine.send(cmd)).await {
                Ok(Ok(status)) => IpcResponse::Ok(status),
                Ok(Err(err)) => IpcResponse::Err(err.to_string()),
                Err(_) => {
                    IpcResponse::Err("timed out waiting for audio engine command".to_string())
                }
            }
        }
    };

    let write_res = tokio::time::timeout(
        CLIENT_IO_TIMEOUT,
        ipc::write_message(&mut stream, &response),
    )
    .await;

    if is_shutdown {
        let _ = shutdown_tx.send(true);
    }

    match write_res {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) if is_client_disconnect(&err) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(TunrError::Ipc(
            "timed out writing IPC response to client".to_string(),
        )),
    }
}

fn map_ipc_to_engine_command(req: IpcRequest) -> EngineCommand {
    match req {
        IpcRequest::Play {
            queue,
            start_index,
            mode,
            playlist_name,
            loop_playback,
        } => EngineCommand::LoadQueue {
            queue,
            start_index,
            mode,
            playlist_name,
            loop_playback,
        },
        IpcRequest::Next => EngineCommand::Next,
        IpcRequest::Prev => EngineCommand::Prev,
        IpcRequest::SeekRelative(delta) => EngineCommand::SeekRelative(delta),
        IpcRequest::SeekAbsolute(target) => EngineCommand::SeekAbsolute(target),
        IpcRequest::TogglePause => EngineCommand::TogglePause,
        IpcRequest::SetPaused(paused) => EngineCommand::SetPaused(paused),
        IpcRequest::SetVolume(vol) => EngineCommand::SetVolume(vol),
        IpcRequest::Stop => EngineCommand::Stop,
        IpcRequest::GetStatus => EngineCommand::GetStatus,
        IpcRequest::Shutdown => EngineCommand::Shutdown,
    }
}

fn is_client_disconnect(err: &TunrError) -> bool {
    match err {
        TunrError::Io(io_err) => {
            if matches!(
                io_err.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
            ) {
                return true;
            }
            #[cfg(windows)]
            if io_err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32) {
                return true;
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;
    use std::process::Command;

    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::audio::null_sink::NullSink;
    use crate::ipc::{PlayMode, PlaybackState};
    use crate::library::Song;

    fn temp_workspace(label: &str) -> PathBuf {
        #[cfg(target_os = "macos")]
        let base = PathBuf::from("/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();

        let dir = base.join(format!(
            "tm-dmn-{label}-{}",
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spawn_short_lived_true() -> io::Result<std::process::Child> {
        #[cfg(windows)]
        {
            Command::new("cmd").args(["/C", "exit 0"]).spawn()
        }
        #[cfg(not(windows))]
        {
            Command::new("true").spawn()
        }
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

    #[tokio::test]
    async fn daemon_ipc_round_trips_playback_controls_and_cleans_up_on_shutdown() {
        let dir = temp_workspace("roundtrip");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let songs = vec![
            make_song(&dir, "Alpha", 2.0),
            make_song(&dir, "Beta", 2.0),
        ];

        let sink = NullSink::with_pacing(Duration::from_millis(8));
        let stats = sink.stats();
        let (ready_tx, ready_rx) = oneshot::channel();
        let daemon_paths = paths.clone();

        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(&daemon_paths, Box::new(sink), Some(ready_tx), None).await
        });

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("daemon should signal readiness")
            .unwrap();

        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(socket_path.exists());
        }
        assert!(paths.pid_path.exists());
        let recorded_pid: u32 = fs::read_to_string(&paths.pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(recorded_pid, std::process::id());

        // Initial GetStatus is Stopped.
        let initial = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(initial.state, PlaybackState::Stopped);
        assert!(initial.current_track.is_none());

        // Next before Play returns clean Daemon error over IPC.
        let err = ipc::send_request(&paths, &IpcRequest::Next)
            .await
            .unwrap_err();
        assert!(matches!(err, TunrError::Daemon(_)));

        // Play starts queue at Alpha.
        let playing = ipc::send_request(
            &paths,
            &IpcRequest::Play {
                queue: songs,
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: Some("DaemonMix".to_string()),
                loop_playback: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(playing.state, PlaybackState::Playing);
        assert!(!playing.loop_playback);
        assert_eq!(playing.playlist_name.as_deref(), Some("DaemonMix"));
        assert_eq!(playing.current_track.as_ref().unwrap().title, "Alpha");
        assert_eq!(playing.queue_index, 0);
        assert_eq!(playing.queue_len, 2);

        // GetStatus reports active playback.
        let polled = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(polled.state, PlaybackState::Playing);
        assert_eq!(polled.current_track.as_ref().unwrap().title, "Alpha");

        // Next advances to Beta; Prev returns to Alpha.
        let next = ipc::send_request(&paths, &IpcRequest::Next).await.unwrap();
        assert_eq!(next.current_track.as_ref().unwrap().title, "Beta");
        assert_eq!(next.queue_index, 1);

        let prev = ipc::send_request(&paths, &IpcRequest::Prev).await.unwrap();
        assert_eq!(prev.current_track.as_ref().unwrap().title, "Alpha");
        assert_eq!(prev.queue_index, 0);
        assert_eq!(prev.position_secs, 0.0);

        // SeekRelative forward then rewind past 0.
        let adv = ipc::send_request(&paths, &IpcRequest::SeekRelative(0.5))
            .await
            .unwrap();
        assert!(adv.position_secs >= 0.4);
        let rew = ipc::send_request(&paths, &IpcRequest::SeekRelative(-5.0))
            .await
            .unwrap();
        assert_eq!(rew.position_secs, 0.0);

        // TogglePause -> Paused -> TogglePause -> Playing.
        let paused = ipc::send_request(&paths, &IpcRequest::TogglePause)
            .await
            .unwrap();
        assert_eq!(paused.state, PlaybackState::Paused);
        let resumed = ipc::send_request(&paths, &IpcRequest::TogglePause)
            .await
            .unwrap();
        assert_eq!(resumed.state, PlaybackState::Playing);

        // Stop -> Stopped.
        let stopped = ipc::send_request(&paths, &IpcRequest::Stop).await.unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert_eq!(stopped.position_secs, 0.0);
        assert!(stopped.current_track.is_none());

        // Shutdown stops daemon and removes socket/pid files.
        let bye = ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        assert_eq!(bye.state, PlaybackState::Stopped);

        daemon_task.await.unwrap().unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(!socket_path.exists());
        }
        assert!(!paths.pid_path.exists());
        assert!(!stats.is_open());
        assert!(stats.flush_count() >= 1);
        assert!(stats.close_count() >= 1);

        // Verify daemon.lock was released after CleanupGuard dropped.
        assert!(
            paths::acquire_file_lock(&paths.daemon_lock_path(), true, false)
                .unwrap()
                .is_some()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn daemon_rejects_second_instance_when_live_pid_owns_socket() {
        let dir = temp_workspace("duplicate");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));

        let (ready_tx, ready_rx) = oneshot::channel();
        let p1 = paths.clone();
        let t1 = tokio::spawn(async move {
            run_daemon_with_sink(&p1, Box::new(NullSink::new()), Some(ready_tx), None).await
        });

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .unwrap()
            .unwrap();

        let dup_err = run_daemon_with_sink(&paths, Box::new(NullSink::new()), None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(dup_err, TunrError::Daemon(ref msg) if msg.contains("already running")),
            "unexpected duplicate daemon error: {dup_err:?}"
        );

        // First daemon's endpoint and PID remain intact and functional.
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(socket_path.exists());
        }
        assert!(paths.pid_path.exists());
        let status = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(status.state, PlaybackState::Stopped);

        ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        t1.await.unwrap().unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(!socket_path.exists());
        }
        assert!(!paths.pid_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn daemon_cleans_stale_socket_and_pid_for_dead_process() {
        let dir = temp_workspace("stale-startup");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        paths.ensure_dirs().unwrap();

        let mut child = spawn_short_lived_true().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        assert!(!ipc::is_pid_alive(dead_pid));

        fs::write(&paths.pid_path, format!("{dead_pid}\n")).unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            fs::write(socket_path, "stale-socket").unwrap();
        }

        let (ready_tx, ready_rx) = oneshot::channel();
        let p = paths.clone();
        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(&p, Box::new(NullSink::new()), Some(ready_tx), None).await
        });

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .unwrap()
            .unwrap();
        let status = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(status.state, PlaybackState::Stopped);

        ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        daemon_task.await.unwrap().unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(!socket_path.exists());
        }
        assert!(!paths.pid_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_get_status_does_not_block_behind_stalled_peer() {
        let dir = temp_workspace("stalled-peer");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let song = make_song(&dir, "Tone", 2.0);

        let sink = NullSink::with_pacing(Duration::from_millis(12));
        let (ready_tx, ready_rx) = oneshot::channel();
        let p = paths.clone();
        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(&p, Box::new(sink), Some(ready_tx), None).await
        });
        ready_rx.await.unwrap();

        ipc::send_request(
            &paths,
            &IpcRequest::Play {
                queue: vec![song],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            },
        )
        .await
        .unwrap();

        // Connect a stalled peer that sends an incomplete JSON line without `\n`.
        let mut stalled = IpcStream::connect(&paths.ipc_endpoint).await.unwrap();
        stalled.write_all(b"{\"Get").await.unwrap();

        // Concurrent GetStatus requests must complete quickly rather than stalling on peer 1.
        let start = tokio::time::Instant::now();
        let mut readers = JoinSet::new();
        for _ in 0..8 {
            let p_clone = paths.clone();
            readers.spawn(async move { ipc::send_request(&p_clone, &IpcRequest::GetStatus).await });
        }
        while let Some(res) = readers.join_next().await {
            let status = res.unwrap().unwrap();
            assert_eq!(status.state, PlaybackState::Playing);
        }
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "GetStatus blocked behind stalled peer ({:?})",
            start.elapsed()
        );

        drop(stalled);
        ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        daemon_task.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn external_watch_shutdown_stops_daemon_cleanly() {
        let dir = temp_workspace("watch-shutdown");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let p = paths.clone();

        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(
                &p,
                Box::new(NullSink::new()),
                Some(ready_tx),
                Some(shutdown_rx),
            )
            .await
        });

        ready_rx.await.unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(socket_path.exists());
        }
        assert!(paths.pid_path.exists());

        shutdown_tx.send(true).unwrap();
        daemon_task.await.unwrap().unwrap();
        #[cfg(unix)]
        {
            let IpcEndpoint::UnixSocket(socket_path) = &paths.ipc_endpoint;
            assert!(!socket_path.exists());
        }
        assert!(!paths.pid_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn daemon_ipc_seek_absolute_zero_rewinds_and_loop_playback_flows_to_status() {
        let dir = temp_workspace("rewind-loop");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let song = make_song(&dir, "LoopSong", 2.0);

        let sink = NullSink::with_pacing(Duration::from_millis(8));
        let (ready_tx, ready_rx) = oneshot::channel();
        let daemon_paths = paths.clone();
        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(&daemon_paths, Box::new(sink), Some(ready_tx), None).await
        });

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("daemon should signal readiness")
            .unwrap();

        let initial = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert!(!initial.loop_playback);

        let playing = ipc::send_request(
            &paths,
            &IpcRequest::Play {
                queue: vec![song],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(playing.state, PlaybackState::Playing);
        assert!(playing.loop_playback);

        let polled = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert!(polled.loop_playback);

        // Advance forward, then rewind to 0.0 while playing.
        let advanced = ipc::send_request(&paths, &IpcRequest::SeekRelative(0.6))
            .await
            .unwrap();
        assert!(advanced.position_secs > 0.4);

        let rewound_playing = ipc::send_request(&paths, &IpcRequest::SeekAbsolute(0.0))
            .await
            .unwrap();
        assert_eq!(rewound_playing.state, PlaybackState::Playing);
        assert_eq!(rewound_playing.position_secs, 0.0);
        assert!(rewound_playing.loop_playback);

        // Advance forward, pause, then rewind to 0.0 while staying paused.
        let advanced_again = ipc::send_request(&paths, &IpcRequest::SeekAbsolute(0.6))
            .await
            .unwrap();
        assert!(advanced_again.position_secs > 0.4);

        let paused = ipc::send_request(&paths, &IpcRequest::SetPaused(true))
            .await
            .unwrap();
        assert_eq!(paused.state, PlaybackState::Paused);
        assert!(paused.position_secs > 0.4);

        let rewound_paused = ipc::send_request(&paths, &IpcRequest::SeekAbsolute(0.0))
            .await
            .unwrap();
        assert_eq!(rewound_paused.state, PlaybackState::Paused);
        assert_eq!(rewound_paused.position_secs, 0.0);
        assert!(rewound_paused.loop_playback);

        let status_while_paused = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(status_while_paused.state, PlaybackState::Paused);
        assert_eq!(status_while_paused.position_secs, 0.0);
        assert!(status_while_paused.loop_playback);

        let stopped = ipc::send_request(&paths, &IpcRequest::Stop).await.unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert!(!stopped.loop_playback);

        ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        daemon_task.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn daemon_startup_defaults_volume_without_config_and_updates_via_ipc() {
        let dir = temp_workspace("volume-default");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        assert!(!paths.config_path.exists());

        let (ready_tx, ready_rx) = oneshot::channel();
        let daemon_paths = paths.clone();
        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(&daemon_paths, Box::new(NullSink::new()), Some(ready_tx), None)
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("daemon should signal readiness")
            .unwrap();

        let initial = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(initial.volume, crate::paths::DEFAULT_VOLUME);
        assert_eq!(initial.volume, 50);

        let updated = ipc::send_request(&paths, &IpcRequest::SetVolume(25))
            .await
            .unwrap();
        assert_eq!(updated.volume, 25);

        let polled = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(polled.volume, 25);

        let err = ipc::send_request(&paths, &IpcRequest::SetVolume(101))
            .await
            .unwrap_err();
        assert!(matches!(err, TunrError::Daemon(_)));

        let still_25 = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(still_25.volume, 25);

        ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        daemon_task.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn daemon_startup_loads_volume_from_workspace_config() {
        let dir = temp_workspace("volume-config");
        let paths = AppPaths::from_roots(dir.join("data"), dir.join("run"));
        let cfg = crate::paths::WorkspaceConfig {
            version: crate::paths::WORKSPACE_CONFIG_VERSION,
            working_dir: paths.working_dir.clone(),
            config_dir: paths.working_dir.join("config"),
            playlists_dir: paths.playlists_dir.clone(),
            music_dir: paths.music_dir.clone(),
            download_dest: None,
            search_limit: None,
            search_provider: None,
            search_format: None,
            volume: Some(75),
        };
        fs::create_dir_all(paths.config_path.parent().unwrap()).unwrap();
        fs::write(&paths.config_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

        let (ready_tx, ready_rx) = oneshot::channel();
        let daemon_paths = paths.clone();
        let daemon_task = tokio::spawn(async move {
            run_daemon_with_sink(&daemon_paths, Box::new(NullSink::new()), Some(ready_tx), None)
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("daemon should signal readiness")
            .unwrap();

        let initial = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(initial.volume, 75);

        let updated = ipc::send_request(&paths, &IpcRequest::SetVolume(25))
            .await
            .unwrap();
        assert_eq!(updated.volume, 25);

        let polled = ipc::send_request(&paths, &IpcRequest::GetStatus)
            .await
            .unwrap();
        assert_eq!(polled.volume, 25);

        ipc::send_request(&paths, &IpcRequest::Shutdown)
            .await
            .unwrap();
        daemon_task.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
