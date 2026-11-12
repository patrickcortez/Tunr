use std::sync::Arc;

use rand::seq::SliceRandom;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{mpsc, oneshot, watch};

use crate::audio::{AudioSink, SymphoniaTrack};
use crate::error::{Result, TermusicError};
use crate::ipc::{PlayMode, PlaybackState, PlaybackStatus};
use crate::library::Song;

const ENGINE_COMMAND_CAPACITY: usize = 64;

/// Control commands accepted by the audio playback engine thread.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineCommand {
    LoadQueue {
        queue: Vec<Song>,
        start_index: Option<usize>,
        mode: PlayMode,
        playlist_name: Option<String>,
        loop_playback: bool,
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

/// Command envelope paired with a Tokio oneshot acknowledgement channel.
pub struct EngineRequest {
    pub command: EngineCommand,
    pub ack: oneshot::Sender<Result<PlaybackStatus>>,
}

/// Async-safe client handle for dispatching commands and reading wait-free playback snapshots.
#[derive(Clone)]
pub struct EngineHandle {
    cmd_tx: mpsc::Sender<EngineRequest>,
    status_rx: watch::Receiver<Arc<PlaybackStatus>>,
}

impl EngineHandle {
    /// Construct a handle from a bounded Tokio command sender and shared status watch receiver.
    pub fn new(
        cmd_tx: mpsc::Sender<EngineRequest>,
        status_rx: watch::Receiver<Arc<PlaybackStatus>>,
    ) -> Self {
        Self { cmd_tx, status_rx }
    }

    /// Return the latest published playback snapshot without blocking on audio decoding or locks.
    pub fn status(&self) -> Arc<PlaybackStatus> {
        Arc::clone(&*self.status_rx.borrow())
    }

    /// Send a command to the audio engine thread and await the resulting status without blocking
    /// the calling Tokio worker thread.
    pub async fn send(&self, command: EngineCommand) -> Result<PlaybackStatus> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.cmd_tx
            .send(EngineRequest {
                command,
                ack: ack_tx,
            })
            .await
            .map_err(|_| TermusicError::Daemon("audio engine is not running".to_string()))?;

        ack_rx
            .await
            .map_err(|_| TermusicError::Daemon("audio engine stopped before responding".to_string()))?
    }
}

/// Stateful queue and decoder engine driving a single [`AudioSink`].
pub struct PlaybackEngine {
    sink: Box<dyn AudioSink>,
    open_spec: Option<(u32, u8)>,
    original_queue: Vec<Song>,
    active_order: Vec<usize>,
    cached_ordered_queue: Vec<Song>,
    cursor: usize,
    history: Vec<usize>,
    mode: PlayMode,
    playlist_name: Option<String>,
    loop_playback: bool,
    state: PlaybackState,
    track: Option<SymphoniaTrack>,
    position_secs: f64,
    paused_position: f64,
    duration_secs: Option<f64>,
    pending_pcm: Vec<f32>,
    pending_pos: f64,
}

impl PlaybackEngine {
    /// Create a new idle engine wrapping `sink`.
    pub fn new(sink: Box<dyn AudioSink>) -> Self {
        Self {
            sink,
            open_spec: None,
            original_queue: Vec::new(),
            active_order: Vec::new(),
            cached_ordered_queue: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            mode: PlayMode::Next,
            playlist_name: None,
            loop_playback: false,
            state: PlaybackState::Stopped,
            track: None,
            position_secs: 0.0,
            paused_position: 0.0,
            duration_secs: None,
            pending_pcm: Vec::new(),
            pending_pos: 0.0,
        }
    }

    /// Return a snapshot of the current [`PlaybackStatus`].
    pub fn status(&self) -> PlaybackStatus {
        if self.state == PlaybackState::Stopped || self.active_order.is_empty() {
            return PlaybackStatus {
                state: PlaybackState::Stopped,
                mode: self.mode,
                playlist_name: None,
                loop_playback: false,
                current_track: None,
                queue_index: 0,
                queue_len: 0,
                position_secs: 0.0,
                duration_secs: None,
                queue: Vec::new(),
            };
        }

        let current_track = self.cached_ordered_queue.get(self.cursor).cloned();

        PlaybackStatus {
            state: self.state,
            mode: self.mode,
            playlist_name: self.playlist_name.clone(),
            loop_playback: self.loop_playback,
            current_track,
            queue_index: self.cursor,
            queue_len: self.active_order.len(),
            position_secs: self.position_secs,
            duration_secs: self.duration_secs,
            queue: self.cached_ordered_queue.clone(),
        }
    }

    /// Current playback state.
    pub fn state(&self) -> PlaybackState {
        self.state
    }

    /// Active playback ordering as indices into `original_queue`.
    pub fn active_order(&self) -> &[usize] {
        &self.active_order
    }

    /// Current cursor position inside `active_order`.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// History stack of previously visited `original_queue` indices.
    pub fn history(&self) -> &[usize] {
        &self.history
    }

    /// Execute a single command and return the updated [`PlaybackStatus`] and whether the engine
    /// should continue running (`false` on [`EngineCommand::Shutdown`]).
    pub fn execute(&mut self, command: EngineCommand) -> (Result<PlaybackStatus>, bool) {
        let mut keep_running = true;
        let outcome = match command {
            EngineCommand::LoadQueue {
                queue,
                start_index,
                mode,
                playlist_name,
                loop_playback,
            } => self.load_queue(queue, start_index, mode, playlist_name, loop_playback),
            EngineCommand::Next => self.next_track(),
            EngineCommand::Prev => self.prev_track(),
            EngineCommand::SeekRelative(delta) => self.seek_relative(delta),
            EngineCommand::SeekAbsolute(target) => self.seek_absolute(target),
            EngineCommand::TogglePause => self.toggle_pause(),
            EngineCommand::SetPaused(paused) => self.set_paused(paused),
            EngineCommand::Stop => self.stop(),
            EngineCommand::GetStatus => Ok(()),
            EngineCommand::Shutdown => {
                keep_running = false;
                self.stop()
            }
        };

        let result = outcome.map(|()| self.status());
        (result, keep_running)
    }

    fn load_queue(
        &mut self,
        queue: Vec<Song>,
        start_index: Option<usize>,
        mode: PlayMode,
        playlist_name: Option<String>,
        loop_playback: bool,
    ) -> Result<()> {
        if queue.is_empty() {
            return Err(TermusicError::InvalidArg(
                "cannot play an empty queue".to_string(),
            ));
        }

        let (active_order, cursor) = build_active_order(queue.len(), start_index, mode);
        let first_song = &queue[active_order[cursor]];
        let track = SymphoniaTrack::open(&first_song.path)?;

        self.pending_pcm.clear();
        self.pending_pos = 0.0;
        if self.open_spec.is_some() {
            let _ = self.sink.flush();
        }
        if let Err(err) = self.ensure_sink_spec(track.sample_rate(), track.channels()) {
            self.stop_clean_on_sink_error();
            return Err(err);
        }

        self.duration_secs = track.duration_secs().or(first_song.duration_secs);
        self.cached_ordered_queue = active_order
            .iter()
            .filter_map(|&idx| queue.get(idx).cloned())
            .collect();
        self.original_queue = queue;
        self.active_order = active_order;
        self.cursor = cursor;
        self.history.clear();
        self.mode = mode;
        self.playlist_name = playlist_name;
        self.loop_playback = loop_playback;
        self.track = Some(track);
        self.position_secs = 0.0;
        self.paused_position = 0.0;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    fn next_track(&mut self) -> Result<()> {
        if self.active_order.is_empty() || self.state == PlaybackState::Stopped {
            return Err(TermusicError::NotFound(
                "no track is currently queued".to_string(),
            ));
        }

        let prev_orig_idx = self.active_order[self.cursor];
        let next_cursor = (self.cursor + 1) % self.active_order.len();
        self.switch_to_cursor(next_cursor, Some(prev_orig_idx), true, false)
    }

    fn prev_track(&mut self) -> Result<()> {
        if self.active_order.is_empty() || self.state == PlaybackState::Stopped {
            return Err(TermusicError::NotFound(
                "no track is currently queued".to_string(),
            ));
        }

        let popped_history = self.history.pop();
        let target_cursor = match popped_history {
            Some(orig_idx) => self
                .active_order
                .iter()
                .position(|&idx| idx == orig_idx)
                .unwrap_or_else(|| self.cursor.saturating_sub(1)),
            None => self.cursor.saturating_sub(1),
        };

        if let Err(err) = self.switch_to_cursor(target_cursor, None, true, false) {
            if let Some(orig_idx) = popped_history {
                self.history.push(orig_idx);
            }
            return Err(err);
        }
        Ok(())
    }

    fn switch_to_cursor(
        &mut self,
        target_cursor: usize,
        push_history: Option<usize>,
        flush_before_switch: bool,
        drain_on_spec_change: bool,
    ) -> Result<()> {
        let (track, duration_secs) = {
            let song = &self.original_queue[self.active_order[target_cursor]];
            let track = SymphoniaTrack::open(&song.path)?;
            let duration_secs = track.duration_secs().or(song.duration_secs);
            (track, duration_secs)
        };
        let next_spec = (track.sample_rate(), track.channels());

        self.pending_pcm.clear();
        self.pending_pos = 0.0;
        if flush_before_switch && self.open_spec.is_some() {
            self.sink.flush()?;
        }
        if drain_on_spec_change && self.open_spec.is_some() && self.open_spec != Some(next_spec) {
            let _ = self.sink.drain();
        }
        if let Err(err) = self.ensure_sink_spec(next_spec.0, next_spec.1) {
            self.stop_clean_on_sink_error();
            return Err(err);
        }

        if let Some(orig_idx) = push_history {
            self.history.push(orig_idx);
        }
        self.cursor = target_cursor;
        self.duration_secs = duration_secs;
        self.track = Some(track);
        self.position_secs = 0.0;
        self.paused_position = 0.0;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    fn seek_relative(&mut self, delta: f64) -> Result<()> {
        if !delta.is_finite() {
            return Err(TermusicError::InvalidArg(
                "seek offset must be a finite number of seconds".to_string(),
            ));
        }
        if self.state == PlaybackState::Stopped || self.track.is_none() {
            return Err(TermusicError::Audio(
                "cannot seek when playback is stopped".to_string(),
            ));
        }

        let target = (self.position_secs + delta).max(0.0);
        if delta > 0.0 {
            if let Some(dur) = self.duration_secs {
                if target >= dur {
                    self.pending_pcm.clear();
                    self.pending_pos = 0.0;
                    if self.open_spec.is_some() {
                        self.sink.flush()?;
                    }
                    return self.advance_or_stop_on_end(false);
                }
            } else {
                self.pending_pcm.clear();
                self.pending_pos = 0.0;
                if self.open_spec.is_some() {
                    self.sink.flush()?;
                }

                let seek_res = self
                    .track
                    .as_mut()
                    .expect("checked above")
                    .seek(target);
                let Ok(actual) = seek_res else {
                    return self.advance_or_stop_on_end(false);
                };

                let probe = match self.track.as_mut().expect("checked above").next_pcm_f32() {
                    Ok(Some((samples, pos_secs))) => {
                        self.pending_pcm.clear();
                        self.pending_pcm.extend_from_slice(samples);
                        self.pending_pos = pos_secs;
                        Ok(true)
                    }
                    Ok(None) => Ok(false),
                    Err(err) => Err(err),
                };
                return match probe {
                    Ok(true) => {
                        self.position_secs = actual;
                        if self.state == PlaybackState::Paused {
                            self.paused_position = actual;
                        }
                        Ok(())
                    }
                    Ok(false) => self.advance_or_stop_on_end(false),
                    Err(err) => Err(err),
                };
            }
        }

        self.seek_track_in_place(target)
    }

    fn seek_absolute(&mut self, target: f64) -> Result<()> {
        if !target.is_finite() {
            return Err(TermusicError::InvalidArg(
                "seek target must be a finite number of seconds".to_string(),
            ));
        }
        if self.state == PlaybackState::Stopped || self.track.is_none() {
            return Err(TermusicError::Audio(
                "cannot seek when playback is stopped".to_string(),
            ));
        }

        let clamped = target.max(0.0);
        self.seek_track_in_place(clamped)
    }

    fn seek_track_in_place(&mut self, target: f64) -> Result<()> {
        if self.track.is_none() {
            return Err(TermusicError::Audio("no active track to seek".to_string()));
        }

        self.pending_pcm.clear();
        self.pending_pos = 0.0;
        if self.open_spec.is_some() {
            self.sink.flush()?;
        }

        let actual = self
            .track
            .as_mut()
            .expect("checked above")
            .seek(target)?;
        self.position_secs = actual;
        if self.state == PlaybackState::Paused {
            self.paused_position = actual;
        }
        Ok(())
    }

    fn toggle_pause(&mut self) -> Result<()> {
        match self.state {
            PlaybackState::Playing => self.set_paused(true),
            PlaybackState::Paused => self.set_paused(false),
            PlaybackState::Stopped => Err(TermusicError::Audio(
                "cannot toggle pause when playback is stopped".to_string(),
            )),
        }
    }

    fn set_paused(&mut self, paused: bool) -> Result<()> {
        match (self.state, paused) {
            (PlaybackState::Stopped, _) => Err(TermusicError::Audio(
                "playback is stopped; start a track with `play` first".to_string(),
            )),
            (PlaybackState::Paused, true) | (PlaybackState::Playing, false) => Ok(()),
            (PlaybackState::Playing, true) => {
                if self.open_spec.is_some() {
                    self.sink.flush()?;
                }
                self.pending_pcm.clear();
                self.pending_pos = 0.0;
                self.paused_position = self.position_secs;
                self.state = PlaybackState::Paused;
                Ok(())
            }
            (PlaybackState::Paused, false) => {
                let target = self.paused_position.max(0.0);
                let (actual, sample_rate, channels) = {
                    let track = self.track.as_mut().ok_or_else(|| {
                        TermusicError::Audio("no active track to resume".to_string())
                    })?;
                    let actual = track.seek(target)?;
                    (actual, track.sample_rate(), track.channels())
                };
                self.ensure_sink_spec(sample_rate, channels)?;
                self.pending_pcm.clear();
                self.pending_pos = 0.0;
                self.position_secs = actual;
                self.state = PlaybackState::Playing;
                Ok(())
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        let mut flush_err = None;
        if self.open_spec.is_some() {
            if let Err(err) = self.sink.flush() {
                flush_err = Some(err);
            }
            self.sink.close();
            self.open_spec = None;
        }
        self.reset_stopped_state();
        match flush_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn advance_or_stop_on_end(&mut self, drain_before_stop: bool) -> Result<()> {
        let next_cursor = self.cursor + 1;
        if next_cursor >= self.active_order.len() {
            if self.loop_playback {
                let prev_orig_idx = self.active_order[self.cursor];
                return self.switch_to_cursor(0, Some(prev_orig_idx), false, drain_before_stop);
            }
            if self.open_spec.is_some() {
                if drain_before_stop {
                    let _ = self.sink.drain();
                }
                self.sink.close();
                self.open_spec = None;
            }
            self.reset_stopped_state();
            return Ok(());
        }

        let prev_orig_idx = self.active_order[self.cursor];
        self.switch_to_cursor(next_cursor, Some(prev_orig_idx), false, drain_before_stop)
    }

    fn ensure_sink_spec(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        let wanted = (sample_rate, channels);
        if self.open_spec == Some(wanted) {
            return Ok(());
        }
        match self.sink.open(sample_rate, channels) {
            Ok(()) => {
                self.open_spec = Some(wanted);
                Ok(())
            }
            Err(err) => {
                self.open_spec = None;
                Err(err)
            }
        }
    }

    fn stop_clean_on_sink_error(&mut self) {
        self.sink.close();
        self.open_spec = None;
        self.reset_stopped_state();
    }

    fn reset_stopped_state(&mut self) {
        self.original_queue.clear();
        self.active_order.clear();
        self.cached_ordered_queue.clear();
        self.cursor = 0;
        self.history.clear();
        self.playlist_name = None;
        self.loop_playback = false;
        self.track = None;
        self.duration_secs = None;
        self.pending_pcm.clear();
        self.pending_pos = 0.0;
        self.position_secs = 0.0;
        self.paused_position = 0.0;
        self.state = PlaybackState::Stopped;
    }
}

fn build_active_order(len: usize, start_index: Option<usize>, mode: PlayMode) -> (Vec<usize>, usize) {
    let mut order: Vec<usize> = (0..len).collect();
    match mode {
        PlayMode::Next => {
            let cursor = start_index.unwrap_or(0).min(len.saturating_sub(1));
            (order, cursor)
        }
        PlayMode::Shuffle => {
            let mut rng = rand::rng();
            match start_index {
                Some(idx) if idx < len => {
                    order.swap(0, idx);
                    order[1..].shuffle(&mut rng);
                }
                _ => {
                    order.shuffle(&mut rng);
                }
            }
            (order, 0)
        }
    }
}

fn publish_status(status_tx: &watch::Sender<Arc<PlaybackStatus>>, status: PlaybackStatus) {
    status_tx.send_replace(Arc::new(status));
}

fn process_command(
    engine: &mut PlaybackEngine,
    status_tx: &watch::Sender<Arc<PlaybackStatus>>,
    req: EngineRequest,
) -> bool {
    let (result, keep_running) = engine.execute(req.command);
    publish_status(status_tx, engine.status());
    let _ = req.ack.send(result);
    keep_running
}

fn drain_pending_commands(
    engine: &mut PlaybackEngine,
    cmd_rx: &mut mpsc::Receiver<EngineRequest>,
    status_tx: &watch::Sender<Arc<PlaybackStatus>>,
) -> bool {
    loop {
        match cmd_rx.try_recv() {
            Ok(req) => {
                if !process_command(engine, status_tx, req) {
                    return false;
                }
            }
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => {
                let _ = engine.stop();
                publish_status(status_tx, engine.status());
                return false;
            }
        }
    }
}

/// Spawn the audio playback engine on a dedicated OS thread (`termusic-engine`), returning an
/// async [`EngineHandle`] and thread [`std::thread::JoinHandle`].
pub fn spawn_engine(sink: Box<dyn AudioSink>) -> Result<(EngineHandle, std::thread::JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel(ENGINE_COMMAND_CAPACITY);
    let (status_tx, status_rx) = watch::channel(Arc::new(PlaybackStatus::default()));

    let handle = std::thread::Builder::new()
        .name("termusic-engine".to_string())
        .spawn(move || run_engine(sink, cmd_rx, status_tx))
        .map_err(|err| {
            TermusicError::Daemon(format!("failed to spawn audio engine thread: {err}"))
        })?;

    Ok((EngineHandle::new(cmd_tx, status_rx), handle))
}

/// Run the audio engine loop on the current dedicated OS thread until [`EngineCommand::Shutdown`]
/// or command channel disconnect.
///
/// Must run outside a Tokio runtime thread because it invokes `cmd_rx.blocking_recv()`,
/// synchronous Symphonia I/O/decoding, and blocking [`AudioSink`] writes.
///
/// While `Playing`, checks `cmd_rx.try_recv()` between every decoded packet BEFORE writing to `sink`
/// so playback controls respond immediately; while `Paused` or `Stopped`, blocks on
/// `cmd_rx.blocking_recv()` with zero busy-spinning.
pub fn run_engine(
    sink: Box<dyn AudioSink>,
    mut cmd_rx: mpsc::Receiver<EngineRequest>,
    status_tx: watch::Sender<Arc<PlaybackStatus>>,
) {
    let mut engine = PlaybackEngine::new(sink);
    publish_status(&status_tx, engine.status());

    loop {
        if engine.state != PlaybackState::Playing {
            match cmd_rx.blocking_recv() {
                Some(req) => {
                    if !process_command(&mut engine, &status_tx, req) {
                        break;
                    }
                    continue;
                }
                None => {
                    let _ = engine.stop();
                    publish_status(&status_tx, engine.status());
                    break;
                }
            }
        }

        if engine.pending_pcm.is_empty() {
            let decode_outcome = if let Some(track) = engine.track.as_mut() {
                match track.next_pcm_f32() {
                    Ok(Some((samples, pos_secs))) => {
                        engine.pending_pcm.clear();
                        engine.pending_pcm.extend_from_slice(samples);
                        engine.pending_pos = pos_secs;
                        Ok(Some(()))
                    }
                    Ok(None) => Ok(None),
                    Err(err) => Err(err),
                }
            } else {
                let _ = engine.stop();
                publish_status(&status_tx, engine.status());
                continue;
            };

            match decode_outcome {
                Ok(Some(())) => {}
                Ok(None) => {
                    if !drain_pending_commands(&mut engine, &mut cmd_rx, &status_tx) {
                        break;
                    }
                    if engine.state == PlaybackState::Playing {
                        if let Err(err) = engine.advance_or_stop_on_end(true) {
                            eprintln!("playback auto-advance error: {err}");
                            let _ = engine.stop();
                        }
                        publish_status(&status_tx, engine.status());
                    }
                    continue;
                }
                Err(err) => {
                    eprintln!("playback decode error: {err}");
                    let _ = engine.stop();
                    publish_status(&status_tx, engine.status());
                    continue;
                }
            }
        }

        // Always inspect pending control commands BEFORE writing the decoded packet to the sink.
        if !drain_pending_commands(&mut engine, &mut cmd_rx, &status_tx) {
            break;
        }
        if engine.state != PlaybackState::Playing || engine.pending_pcm.is_empty() {
            continue;
        }

        let Some((sample_rate, channels)) = engine
            .track
            .as_ref()
            .map(|t| (t.sample_rate(), t.channels()))
        else {
            let _ = engine.stop();
            publish_status(&status_tx, engine.status());
            continue;
        };

        if let Err(err) = engine.ensure_sink_spec(sample_rate, channels) {
            eprintln!("audio sink open error: {err}");
            engine.stop_clean_on_sink_error();
            publish_status(&status_tx, engine.status());
            continue;
        }

        if let Err(err) = engine.sink.write_f32(&engine.pending_pcm) {
            eprintln!("audio sink write error: {err}");
            engine.stop_clean_on_sink_error();
            publish_status(&status_tx, engine.status());
            continue;
        }

        let frames = engine.pending_pcm.len() / usize::from(channels.max(1));
        let packet_secs = frames as f64 / f64::from(sample_rate.max(1));
        let mut new_pos = engine.pending_pos + packet_secs;
        if let Some(dur) = engine.duration_secs {
            new_pos = new_pos.min(dur);
        }
        engine.position_secs = new_pos;
        engine.pending_pcm.clear();
        publish_status(&status_tx, engine.status());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::f32::consts::PI;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::audio::null_sink::NullSink;

    fn temp_workspace(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-player-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pcm_wav_opts(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        duration_secs: f64,
        unknown_len: bool,
    ) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let bits_per_sample: u16 = 16;
        let num_frames = (sample_rate as f64 * duration_secs).round() as u32;
        let block_align = channels * (bits_per_sample / 8);
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = num_frames * u32::from(block_align);
        let riff_chunk_size = if unknown_len { u32::MAX } else { 36 + data_size };
        let data_chunk_size = if unknown_len { u32::MAX } else { data_size };

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
        buf.extend_from_slice(&data_chunk_size.to_le_bytes());

        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            for ch in 0..channels {
                let freq = 440.0 + f32::from(ch) * 110.0;
                let sample = ((2.0 * PI * freq * t).sin() * 0.25 * i16::MAX as f32) as i16;
                buf.extend_from_slice(&sample.to_le_bytes());
            }
        }

        fs::write(path, buf).unwrap();
    }

    fn make_song(dir: &Path, name: &str, sample_rate: u32, channels: u16, duration_secs: f64) -> Song {
        let path = dir.join(format!("{name}.wav"));
        write_pcm_wav_opts(&path, sample_rate, channels, duration_secs, false);
        Song {
            id: format!("id-{name}"),
            title: name.to_string(),
            artist: Some("Test Artist".to_string()),
            album: None,
            path,
            duration_secs: Some(duration_secs),
        }
    }

    fn make_streaming_song(
        dir: &Path,
        name: &str,
        sample_rate: u32,
        channels: u16,
        duration_secs: f64,
    ) -> Song {
        let path = dir.join(format!("{name}.wav"));
        write_pcm_wav_opts(&path, sample_rate, channels, duration_secs, true);
        Song {
            id: format!("id-{name}"),
            title: name.to_string(),
            artist: Some("Test Artist".to_string()),
            album: None,
            path,
            duration_secs: None,
        }
    }

    async fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for playback condition after {timeout:?}");
    }

    async fn join_engine(join_handle: std::thread::JoinHandle<()>) {
        tokio::task::spawn_blocking(move || join_handle.join())
            .await
            .expect("join blocking task should succeed")
            .expect("engine thread should exit cleanly");
    }

    #[test]
    fn load_queue_next_mode_and_explicit_next_wraps_cleanly() {
        let dir = temp_workspace("next-wrap");
        let songs = vec![
            make_song(&dir, "TrackA", 44_100, 1, 0.5),
            make_song(&dir, "TrackB", 44_100, 1, 0.5),
            make_song(&dir, "TrackC", 44_100, 1, 0.5),
        ];

        let sink = NullSink::new();
        let stats = sink.stats();
        let mut engine = PlaybackEngine::new(Box::new(sink));

        let (res, keep) = engine.execute(EngineCommand::LoadQueue {
            queue: songs,
            start_index: Some(1),
            mode: PlayMode::Next,
            playlist_name: Some("Mix".to_string()),
            loop_playback: false,
        });
        assert!(keep);
        let status = res.unwrap();
        assert_eq!(status.state, PlaybackState::Playing);
        assert_eq!(status.mode, PlayMode::Next);
        assert_eq!(status.playlist_name.as_deref(), Some("Mix"));
        assert_eq!(status.queue_index, 1);
        assert_eq!(status.queue_len, 3);
        assert_eq!(status.current_track.as_ref().unwrap().title, "TrackB");
        assert_eq!(engine.active_order(), &[0, 1, 2]);
        assert!(engine.history().is_empty());

        let status = engine.execute(EngineCommand::Next).0.unwrap();
        assert_eq!(status.queue_index, 2);
        assert_eq!(status.current_track.as_ref().unwrap().title, "TrackC");
        assert_eq!(engine.history(), &[1]);

        // Explicit Next at queue end wraps back to index 0.
        let status = engine.execute(EngineCommand::Next).0.unwrap();
        assert_eq!(status.queue_index, 0);
        assert_eq!(status.current_track.as_ref().unwrap().title, "TrackA");
        assert_eq!(engine.history(), &[1, 2]);

        // Same audio spec across all tracks must open the sink only once.
        assert_eq!(stats.open_calls(), 1);
        assert_eq!(stats.opened_specs(), vec![(44_100, 1)]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shuffle_mode_pins_start_track_at_position_zero() {
        let dir = temp_workspace("shuffle-pin");
        let songs: Vec<Song> = (0..5)
            .map(|i| make_song(&dir, &format!("S{i}"), 44_100, 1, 0.2))
            .collect();

        let sink = NullSink::new();
        let mut engine = PlaybackEngine::new(Box::new(sink));
        let mut saw_non_trivial_order = false;

        for _ in 0..16 {
            let (res, _) = engine.execute(EngineCommand::LoadQueue {
                queue: songs.clone(),
                start_index: Some(3),
                mode: PlayMode::Shuffle,
                playlist_name: None,
                loop_playback: false,
            });
            let status = res.unwrap();
            assert_eq!(status.mode, PlayMode::Shuffle);
            assert_eq!(status.queue_index, 0);
            assert_eq!(status.current_track.as_ref().unwrap().title, "S3");
            assert_eq!(engine.active_order()[0], 3);

            let uniq: HashSet<usize> = engine.active_order().iter().copied().collect();
            assert_eq!(uniq.len(), 5);
            if engine.active_order() != [3, 0, 1, 2, 4] && engine.active_order() != [3, 1, 2, 0, 4] {
                saw_non_trivial_order = true;
            }
        }
        assert!(saw_non_trivial_order, "shuffle tail never varied across 16 runs");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shuffle_mode_with_none_start_index_randomizes_track_zero() {
        let dir = temp_workspace("shuffle-none");
        let songs: Vec<Song> = (0..5)
            .map(|i| make_song(&dir, &format!("R{i}"), 44_100, 1, 0.2))
            .collect();

        let mut engine = PlaybackEngine::new(Box::new(NullSink::new()));
        let mut first_indices = HashSet::new();

        for _ in 0..32 {
            let (res, _) = engine.execute(EngineCommand::LoadQueue {
                queue: songs.clone(),
                start_index: None,
                mode: PlayMode::Shuffle,
                playlist_name: Some("All".to_string()),
                loop_playback: false,
            });
            let status = res.unwrap();
            assert_eq!(status.mode, PlayMode::Shuffle);
            assert_eq!(status.queue_index, 0);

            let uniq: HashSet<usize> = engine.active_order().iter().copied().collect();
            assert_eq!(uniq.len(), 5);
            first_indices.insert(engine.active_order()[0]);
        }

        assert!(
            first_indices.len() > 1,
            "playlist shuffle with start_index: None should vary index 0 across runs, got {first_indices:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prev_pops_history_stack_then_clamps_cursor_to_zero() {
        let dir = temp_workspace("prev-history");
        let songs: Vec<Song> = (0..4)
            .map(|i| make_song(&dir, &format!("T{i}"), 44_100, 1, 0.3))
            .collect();

        let mut engine = PlaybackEngine::new(Box::new(NullSink::new()));
        engine
            .execute(EngineCommand::LoadQueue {
                queue: songs.clone(),
                start_index: Some(2),
                mode: PlayMode::Shuffle,
                playlist_name: None,
                loop_playback: false,
            })
            .0
            .unwrap();

        let order = engine.active_order().to_vec();
        assert_eq!(order[0], 2);

        let s1 = engine.execute(EngineCommand::Next).0.unwrap();
        assert_eq!(s1.queue_index, 1);
        assert_eq!(s1.current_track.as_ref().unwrap().title, songs[order[1]].title);

        let s2 = engine.execute(EngineCommand::Next).0.unwrap();
        assert_eq!(s2.queue_index, 2);
        assert_eq!(s2.current_track.as_ref().unwrap().title, songs[order[2]].title);

        // Prev walks back through history stack.
        let p1 = engine.execute(EngineCommand::Prev).0.unwrap();
        assert_eq!(p1.queue_index, 1);
        assert_eq!(p1.position_secs, 0.0);
        assert_eq!(p1.current_track.as_ref().unwrap().title, songs[order[1]].title);

        let p2 = engine.execute(EngineCommand::Prev).0.unwrap();
        assert_eq!(p2.queue_index, 0);
        assert_eq!(p2.position_secs, 0.0);
        assert_eq!(p2.current_track.as_ref().unwrap().title, "T2");
        assert!(engine.history().is_empty());

        // History empty at cursor 0 clamps to 0.
        let p3 = engine.execute(EngineCommand::Prev).0.unwrap();
        assert_eq!(p3.queue_index, 0);
        assert_eq!(p3.current_track.as_ref().unwrap().title, "T2");

        // Next mode starting mid-queue with empty history steps cursor - 1 down to 0.
        engine
            .execute(EngineCommand::LoadQueue {
                queue: songs,
                start_index: Some(2),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .0
            .unwrap();
        let back1 = engine.execute(EngineCommand::Prev).0.unwrap();
        assert_eq!(back1.queue_index, 1);
        assert_eq!(back1.current_track.as_ref().unwrap().title, "T1");
        let back2 = engine.execute(EngineCommand::Prev).0.unwrap();
        assert_eq!(back2.queue_index, 0);
        assert_eq!(back2.current_track.as_ref().unwrap().title, "T0");
        let back3 = engine.execute(EngineCommand::Prev).0.unwrap();
        assert_eq!(back3.queue_index, 0);
        assert_eq!(back3.current_track.as_ref().unwrap().title, "T0");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_relative_clamps_rewind_to_zero_and_advances_or_stops_past_eof() {
        let dir = temp_workspace("seek-rel");
        let songs = vec![
            make_song(&dir, "First", 44_100, 1, 1.0),
            make_song(&dir, "Second", 44_100, 1, 1.0),
        ];

        let sink = NullSink::new();
        let stats = sink.stats();
        let mut engine = PlaybackEngine::new(Box::new(sink));

        engine
            .execute(EngineCommand::LoadQueue {
                queue: songs,
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .0
            .unwrap();

        let seeked = engine.execute(EngineCommand::SeekAbsolute(0.5)).0.unwrap();
        assert!(seeked.position_secs > 0.35 && seeked.position_secs <= 0.51);

        // Rewind past start clamps cleanly to 0.0.
        let rewound = engine.execute(EngineCommand::SeekRelative(-10.0)).0.unwrap();
        assert_eq!(rewound.position_secs, 0.0);
        assert_eq!(rewound.current_track.as_ref().unwrap().title, "First");

        // Advance past EOF on first track moves to second track.
        let advanced = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(advanced.state, PlaybackState::Playing);
        assert_eq!(advanced.queue_index, 1);
        assert_eq!(advanced.position_secs, 0.0);
        assert_eq!(advanced.current_track.as_ref().unwrap().title, "Second");

        // Advance past EOF on last track stops cleanly.
        let stopped = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert!(stopped.current_track.is_none());
        assert_eq!(stopped.position_secs, 0.0);
        assert_eq!(stats.close_count(), 1);
        assert!(stats.flush_count() >= 4);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_relative_advances_and_stops_when_duration_is_none() {
        let dir = temp_workspace("seek-unknown-dur");
        let stream_songs = vec![
            make_streaming_song(&dir, "Stream1", 44_100, 1, 0.5),
            make_streaming_song(&dir, "Stream2", 44_100, 1, 0.5),
        ];

        let sink = NullSink::new();
        let stats = sink.stats();
        let mut engine = PlaybackEngine::new(Box::new(sink));

        // 1. Streaming WAV has duration_secs == None natively and hits EOF on next_pcm_f32() after seeking past file end.
        let loaded = engine
            .execute(EngineCommand::LoadQueue {
                queue: stream_songs,
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .0
            .unwrap();
        assert!(loaded.duration_secs.is_none());

        let mid = engine.execute(EngineCommand::SeekRelative(0.2)).0.unwrap();
        assert_eq!(mid.current_track.as_ref().unwrap().title, "Stream1");
        assert!(mid.position_secs > 0.05 && mid.position_secs <= 0.25);

        let advanced = engine.execute(EngineCommand::SeekRelative(5.0)).0.unwrap();
        assert_eq!(advanced.state, PlaybackState::Playing);
        assert_eq!(advanced.queue_index, 1);
        assert_eq!(advanced.current_track.as_ref().unwrap().title, "Stream2");

        let stopped = engine.execute(EngineCommand::SeekRelative(5.0)).0.unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert!(stopped.current_track.is_none());
        assert_eq!(stats.close_count(), 1);

        // 2. When track.seek() errors (e.g. OutOfRange) and duration_secs is None, advance/stop cleanly too.
        let bounded = vec![
            make_song(&dir, "Bounded1", 44_100, 1, 0.5),
            make_song(&dir, "Bounded2", 44_100, 1, 0.5),
        ];
        engine
            .execute(EngineCommand::LoadQueue {
                queue: bounded,
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .0
            .unwrap();
        engine.duration_secs = None;
        let adv2 = engine.execute(EngineCommand::SeekRelative(5.0)).0.unwrap();
        assert_eq!(adv2.state, PlaybackState::Playing);
        assert_eq!(adv2.current_track.as_ref().unwrap().title, "Bounded2");

        engine.duration_secs = None;
        let stop2 = engine.execute(EngineCommand::SeekRelative(5.0)).0.unwrap();
        assert_eq!(stop2.state, PlaybackState::Stopped);
        assert_eq!(stats.close_count(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pause_flushes_and_resume_seeks_to_paused_position_without_audio_loss() {
        let dir = temp_workspace("pause-resume");
        let song = make_song(&dir, "LongTone", 44_100, 1, 1.5);
        let expected_samples = 66_150u64; // 1.5s * 44100Hz * 1ch

        let sink = NullSink::with_pacing(Duration::from_millis(6));
        let stats = sink.stats();
        let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

        let started = handle
            .send(EngineCommand::LoadQueue {
                queue: vec![song],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .await
            .unwrap();
        assert_eq!(started.state, PlaybackState::Playing);

        wait_until(Duration::from_secs(3), || {
            handle.status().position_secs >= 0.18
        })
        .await;

        let paused = handle.send(EngineCommand::TogglePause).await.unwrap();
        assert_eq!(paused.state, PlaybackState::Paused);
        let paused_pos = paused.position_secs;
        assert!(paused_pos >= 0.15, "expected progress before pause, got {paused_pos}");
        assert!(stats.flush_count() >= 1);

        let written_during_pause = stats.written_samples();
        tokio::time::sleep(Duration::from_millis(45)).await;
        // Engine must block on cmd_rx.blocking_recv() while paused: zero extra writes or busy spin.
        assert_eq!(stats.written_samples(), written_during_pause);

        let resumed = handle.send(EngineCommand::SetPaused(false)).await.unwrap();
        assert_eq!(resumed.state, PlaybackState::Playing);
        assert!(
            resumed.position_secs <= paused_pos + 0.001,
            "resume seeked to {} after pausing at {}",
            resumed.position_secs,
            paused_pos
        );

        wait_until(Duration::from_secs(5), || {
            handle.status().state == PlaybackState::Stopped
        })
        .await;

        assert!(stats.drain_count() >= 1);
        assert_eq!(stats.close_count(), 1);
        assert!(
            stats.written_samples() >= expected_samples,
            "lost audio across pause/resume: wrote {} < expected {}",
            stats.written_samples(),
            expected_samples
        );

        handle.send(EngineCommand::Shutdown).await.unwrap();
        join_engine(join_handle).await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_eof_reopens_sink_only_on_spec_change_and_stops_cleanly() {
        let dir = temp_workspace("auto-eof");
        let t1 = make_song(&dir, "Mono44k", 44_100, 1, 0.2);
        let t2 = make_song(&dir, "Mono44kAgain", 44_100, 1, 0.2);
        let t3 = make_song(&dir, "Stereo48k", 48_000, 2, 0.2);
        let expected_samples = 8_820 + 8_820 + 19_200;

        let sink = NullSink::new();
        let stats = sink.stats();
        let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

        handle
            .send(EngineCommand::LoadQueue {
                queue: vec![t1, t2, t3],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: Some("AutoPlay".to_string()),
                loop_playback: false,
            })
            .await
            .unwrap();

        wait_until(Duration::from_secs(3), || {
            handle.status().state == PlaybackState::Stopped
        })
        .await;

        let final_status = handle.status();
        assert_eq!(final_status.state, PlaybackState::Stopped);
        assert!(final_status.current_track.is_none());
        assert_eq!(final_status.position_secs, 0.0);
        assert_eq!(stats.opened_specs(), vec![(44_100, 1), (48_000, 2)]);
        assert_eq!(stats.open_calls(), 2);
        assert!(stats.drain_count() >= 2);
        assert_eq!(stats.close_count(), 1);
        assert!(!stats.is_open());
        assert_eq!(stats.written_samples(), expected_samples);

        handle.send(EngineCommand::Shutdown).await.unwrap();
        join_engine(join_handle).await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commands_preempt_decoding_immediately_between_packets() {
        let dir = temp_workspace("preempt");
        let long_song = make_song(&dir, "ThreeSeconds", 44_100, 1, 3.0);

        let sink = NullSink::with_pacing(Duration::from_millis(8));
        let stats = sink.stats();
        let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

        handle
            .send(EngineCommand::LoadQueue {
                queue: vec![long_song],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .await
            .unwrap();

        wait_until(Duration::from_secs(2), || stats.write_calls() >= 2).await;

        let start = tokio::time::Instant::now();
        let stopped = handle.send(EngineCommand::Stop).await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(200),
            "stop took too long ({elapsed:?}), command preemption failed"
        );
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert!(stats.flush_count() >= 1);
        assert_eq!(stats.close_count(), 1);
        assert!(stats.written_samples() < 44_100 * 3);

        handle.send(EngineCommand::Shutdown).await.unwrap();
        join_engine(join_handle).await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_absolute_zero_rewinds_while_playing_and_paused_and_errors_when_stopped() {
        let dir = temp_workspace("seek-abs-zero");
        let songs = vec![make_song(&dir, "Tone", 44_100, 1, 1.0)];
        let mut engine = PlaybackEngine::new(Box::new(NullSink::new()));

        let err = engine.execute(EngineCommand::SeekAbsolute(0.0)).0.unwrap_err();
        assert!(
            matches!(err, TermusicError::Audio(ref msg) if msg == "cannot seek when playback is stopped"),
            "unexpected error when seeking stopped engine: {err:?}"
        );

        engine
            .execute(EngineCommand::LoadQueue {
                queue: songs,
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .0
            .unwrap();

        let seeked = engine.execute(EngineCommand::SeekAbsolute(0.5)).0.unwrap();
        assert!(seeked.position_secs > 0.35 && seeked.position_secs <= 0.51);

        let rewound_playing = engine.execute(EngineCommand::SeekAbsolute(0.0)).0.unwrap();
        assert_eq!(rewound_playing.state, PlaybackState::Playing);
        assert_eq!(rewound_playing.position_secs, 0.0);

        let seeked_again = engine.execute(EngineCommand::SeekAbsolute(0.5)).0.unwrap();
        assert!(seeked_again.position_secs > 0.35);
        let paused = engine.execute(EngineCommand::SetPaused(true)).0.unwrap();
        assert_eq!(paused.state, PlaybackState::Paused);
        assert!(paused.position_secs > 0.35);

        let rewound_paused = engine.execute(EngineCommand::SeekAbsolute(0.0)).0.unwrap();
        assert_eq!(rewound_paused.state, PlaybackState::Paused);
        assert_eq!(rewound_paused.position_secs, 0.0);
        assert_eq!(engine.paused_position, 0.0);

        let resumed = engine.execute(EngineCommand::SetPaused(false)).0.unwrap();
        assert_eq!(resumed.state, PlaybackState::Playing);
        assert_eq!(resumed.position_secs, 0.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_playback_wraps_on_seek_relative_past_eof_for_next_shuffle_and_single_song() {
        let dir = temp_workspace("loop-seek-eof");
        let songs = vec![
            make_song(&dir, "First", 44_100, 1, 0.5),
            make_song(&dir, "Second", 44_100, 1, 0.5),
            make_song(&dir, "Third", 44_100, 1, 0.5),
        ];

        let mut engine = PlaybackEngine::new(Box::new(NullSink::new()));

        // 1. Multi-track queue in Next mode.
        let loaded = engine
            .execute(EngineCommand::LoadQueue {
                queue: songs.clone(),
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: Some("LoopMix".to_string()),
                loop_playback: true,
            })
            .0
            .unwrap();
        assert!(loaded.loop_playback);
        assert_eq!(loaded.queue_index, 0);

        let s1 = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(s1.queue_index, 1);
        assert_eq!(s1.current_track.as_ref().unwrap().title, "Second");
        assert!(s1.loop_playback);

        let s2 = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(s2.queue_index, 2);
        assert_eq!(s2.current_track.as_ref().unwrap().title, "Third");
        assert!(s2.loop_playback);

        // Past EOF at queue end wraps cleanly back to cursor 0.
        let wrapped = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(wrapped.state, PlaybackState::Playing);
        assert_eq!(wrapped.queue_index, 0);
        assert_eq!(wrapped.position_secs, 0.0);
        assert_eq!(wrapped.current_track.as_ref().unwrap().title, "First");
        assert!(wrapped.loop_playback);

        // 2. Multi-track queue in Shuffle mode.
        let shuffled = engine
            .execute(EngineCommand::LoadQueue {
                queue: songs.clone(),
                start_index: Some(1),
                mode: PlayMode::Shuffle,
                playlist_name: None,
                loop_playback: true,
            })
            .0
            .unwrap();
        assert!(shuffled.loop_playback);
        let order = engine.active_order().to_vec();
        assert_eq!(shuffled.current_track.as_ref().unwrap().title, songs[order[0]].title);

        let sh1 = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(sh1.queue_index, 1);
        assert_eq!(sh1.current_track.as_ref().unwrap().title, songs[order[1]].title);

        let sh2 = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(sh2.queue_index, 2);
        assert_eq!(sh2.current_track.as_ref().unwrap().title, songs[order[2]].title);

        let sh_wrapped = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(sh_wrapped.state, PlaybackState::Playing);
        assert_eq!(sh_wrapped.queue_index, 0);
        assert_eq!(sh_wrapped.position_secs, 0.0);
        assert_eq!(
            sh_wrapped.current_track.as_ref().unwrap().title,
            songs[order[0]].title
        );
        assert!(sh_wrapped.loop_playback);

        // 3. Single-song queue.
        let single_loaded = engine
            .execute(EngineCommand::LoadQueue {
                queue: vec![songs[0].clone()],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: true,
            })
            .0
            .unwrap();
        assert!(single_loaded.loop_playback);

        let single_wrapped = engine.execute(EngineCommand::SeekRelative(2.0)).0.unwrap();
        assert_eq!(single_wrapped.state, PlaybackState::Playing);
        assert_eq!(single_wrapped.queue_index, 0);
        assert_eq!(single_wrapped.position_secs, 0.0);
        assert_eq!(single_wrapped.current_track.as_ref().unwrap().title, "First");
        assert!(single_wrapped.loop_playback);

        // Stop clears loop_playback back to false.
        let stopped = engine.execute(EngineCommand::Stop).0.unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert!(!stopped.loop_playback);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn loop_playback_true_wraps_on_auto_eof_for_next_shuffle_and_single_song() {
        let dir = temp_workspace("loop-auto-eof");
        let t1 = make_song(&dir, "LoopA", 44_100, 1, 0.15);
        let t2 = make_song(&dir, "LoopB", 44_100, 1, 0.15);
        let single = make_song(&dir, "Solo", 44_100, 1, 0.15);
        let two_track_samples = 6_615u64 * 2;
        let single_track_samples = 6_615u64;

        // 1. Multi-track Next mode.
        {
            let sink = NullSink::new();
            let stats = sink.stats();
            let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

            let loaded = handle
                .send(EngineCommand::LoadQueue {
                    queue: vec![t1.clone(), t2.clone()],
                    start_index: Some(0),
                    mode: PlayMode::Next,
                    playlist_name: Some("LoopNext".to_string()),
                    loop_playback: true,
                })
                .await
                .unwrap();
            assert!(loaded.loop_playback);

            wait_until(Duration::from_secs(3), || {
                stats.written_samples() >= two_track_samples * 2 + 1_000
            })
            .await;

            let status = handle.status();
            assert_eq!(status.state, PlaybackState::Playing);
            assert!(status.loop_playback);
            assert_eq!(stats.close_count(), 0);

            let stopped = handle.send(EngineCommand::Stop).await.unwrap();
            assert_eq!(stopped.state, PlaybackState::Stopped);
            assert!(!stopped.loop_playback);

            handle.send(EngineCommand::Shutdown).await.unwrap();
            join_engine(join_handle).await;
        }

        // 2. Multi-track Shuffle mode.
        {
            let sink = NullSink::new();
            let stats = sink.stats();
            let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

            let loaded = handle
                .send(EngineCommand::LoadQueue {
                    queue: vec![t1.clone(), t2.clone()],
                    start_index: Some(0),
                    mode: PlayMode::Shuffle,
                    playlist_name: None,
                    loop_playback: true,
                })
                .await
                .unwrap();
            assert!(loaded.loop_playback);

            wait_until(Duration::from_secs(3), || {
                stats.written_samples() >= two_track_samples * 2 + 1_000
            })
            .await;

            let status = handle.status();
            assert_eq!(status.state, PlaybackState::Playing);
            assert!(status.loop_playback);
            assert_eq!(stats.close_count(), 0);

            handle.send(EngineCommand::Shutdown).await.unwrap();
            join_engine(join_handle).await;
        }

        // 3. Single-song queue.
        {
            let sink = NullSink::new();
            let stats = sink.stats();
            let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

            let loaded = handle
                .send(EngineCommand::LoadQueue {
                    queue: vec![single],
                    start_index: Some(0),
                    mode: PlayMode::Next,
                    playlist_name: None,
                    loop_playback: true,
                })
                .await
                .unwrap();
            assert!(loaded.loop_playback);

            wait_until(Duration::from_secs(3), || {
                stats.written_samples() >= single_track_samples * 3 + 500
            })
            .await;

            let status = handle.status();
            assert_eq!(status.state, PlaybackState::Playing);
            assert!(status.loop_playback);
            assert_eq!(status.queue_index, 0);
            assert_eq!(stats.close_count(), 0);

            handle.send(EngineCommand::Shutdown).await.unwrap();
            join_engine(join_handle).await;
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn loop_playback_false_stops_at_end_on_auto_eof() {
        let dir = temp_workspace("no-loop-auto-eof");
        let t1 = make_song(&dir, "EndA", 44_100, 1, 0.15);
        let t2 = make_song(&dir, "EndB", 44_100, 1, 0.15);
        let expected_samples = 6_615u64 * 2;

        let sink = NullSink::new();
        let stats = sink.stats();
        let (handle, join_handle) = spawn_engine(Box::new(sink)).unwrap();

        let loaded = handle
            .send(EngineCommand::LoadQueue {
                queue: vec![t1, t2],
                start_index: Some(0),
                mode: PlayMode::Next,
                playlist_name: None,
                loop_playback: false,
            })
            .await
            .unwrap();
        assert!(!loaded.loop_playback);

        wait_until(Duration::from_secs(3), || {
            handle.status().state == PlaybackState::Stopped
        })
        .await;

        let final_status = handle.status();
        assert_eq!(final_status.state, PlaybackState::Stopped);
        assert!(!final_status.loop_playback);
        assert_eq!(stats.close_count(), 1);
        assert_eq!(stats.written_samples(), expected_samples);

        handle.send(EngineCommand::Shutdown).await.unwrap();
        join_engine(join_handle).await;
        let _ = fs::remove_dir_all(&dir);
    }
}
