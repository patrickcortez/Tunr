use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::audio::AudioSink;
use crate::error::{Result, TunrError};

#[derive(Debug, Default)]
struct NullSinkSpecState {
    is_open: bool,
    current_spec: Option<(u32, u8)>,
    opened_specs: Vec<(u32, u8)>,
}

/// Shared thread-safe instrumentation counters for [`NullSink`].
///
/// Tests can clone this `Arc<NullSinkStats>` via [`NullSink::stats`] before moving
/// the sink into a playback thread and inspect opened stream specs, written sample
/// counts, flushes, drains, and closes. Poisoned mutex guards recover via
/// `into_inner()` rather than panicking across threads.
#[derive(Debug, Default)]
pub struct NullSinkStats {
    state: Mutex<NullSinkSpecState>,
    is_open: AtomicBool,
    open_calls: AtomicU64,
    written_samples: AtomicU64,
    write_calls: AtomicU64,
    flush_count: AtomicU64,
    drain_count: AtomicU64,
    close_count: AtomicU64,
}

impl NullSinkStats {
    /// Create a fresh zeroed stats instance.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_state(&self) -> MutexGuard<'_, NullSinkSpecState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Return all `(sample_rate, channels)` specs for which the sink opened or reopened.
    pub fn opened_specs(&self) -> Vec<(u32, u8)> {
        self.lock_state().opened_specs.clone()
    }

    /// Return the currently open `(sample_rate, channels)` spec, if any.
    pub fn current_spec(&self) -> Option<(u32, u8)> {
        self.lock_state().current_spec
    }

    /// Return whether the sink is currently open.
    pub fn is_open(&self) -> bool {
        self.is_open.load(Ordering::SeqCst)
    }

    /// Return the total number of `open` calls (including idempotent no-op calls).
    pub fn open_calls(&self) -> u64 {
        self.open_calls.load(Ordering::SeqCst)
    }

    /// Return the total number of `f32` samples passed to `write_f32`.
    pub fn written_samples(&self) -> u64 {
        self.written_samples.load(Ordering::SeqCst)
    }

    /// Return the total number of non-empty `write_f32` calls.
    pub fn write_calls(&self) -> u64 {
        self.write_calls.load(Ordering::SeqCst)
    }

    /// Return the total number of `flush` calls.
    pub fn flush_count(&self) -> u64 {
        self.flush_count.load(Ordering::SeqCst)
    }

    /// Return the total number of `drain` calls.
    pub fn drain_count(&self) -> u64 {
        self.drain_count.load(Ordering::SeqCst)
    }

    /// Return the total number of `close` transitions.
    pub fn close_count(&self) -> u64 {
        self.close_count.load(Ordering::SeqCst)
    }

    /// Clear recorded counters and spec history while preserving current open state.
    pub fn reset(&self) {
        let mut state = self.lock_state();
        state.opened_specs.clear();
        self.open_calls.store(0, Ordering::SeqCst);
        self.written_samples.store(0, Ordering::SeqCst);
        self.write_calls.store(0, Ordering::SeqCst);
        self.flush_count.store(0, Ordering::SeqCst);
        self.drain_count.store(0, Ordering::SeqCst);
        self.close_count.store(0, Ordering::SeqCst);
    }
}

/// Headless [`AudioSink`] implementation for automated tests.
///
/// Records stream lifecycle events and written sample counts into a shared
/// [`NullSinkStats`] handle, with optional per-write pacing delay.
#[derive(Debug, Clone)]
pub struct NullSink {
    stats: Arc<NullSinkStats>,
    pacing: Option<Duration>,
}

impl Default for NullSink {
    fn default() -> Self {
        Self::new()
    }
}

impl NullSink {
    /// Create a `NullSink` with zero pacing delay and fresh counters.
    pub fn new() -> Self {
        Self {
            stats: Arc::new(NullSinkStats::new()),
            pacing: None,
        }
    }

    /// Create a `NullSink` that sleeps for `pacing` on each non-empty write.
    pub fn with_pacing(pacing: Duration) -> Self {
        Self {
            stats: Arc::new(NullSinkStats::new()),
            pacing: (!pacing.is_zero()).then_some(pacing),
        }
    }

    /// Create a `NullSink` backed by an externally constructed [`NullSinkStats`].
    pub fn with_stats(stats: Arc<NullSinkStats>) -> Self {
        Self {
            stats,
            pacing: None,
        }
    }

    /// Update the optional per-write pacing delay.
    pub fn set_pacing(&mut self, pacing: Option<Duration>) {
        self.pacing = pacing.filter(|d| !d.is_zero());
    }

    /// Return a shared handle to this sink's instrumentation counters.
    pub fn stats(&self) -> Arc<NullSinkStats> {
        Arc::clone(&self.stats)
    }

    /// Return all `(sample_rate, channels)` specs for which the sink opened or reopened.
    pub fn opened_specs(&self) -> Vec<(u32, u8)> {
        self.stats.opened_specs()
    }

    /// Return the currently open `(sample_rate, channels)` spec, if any.
    pub fn current_spec(&self) -> Option<(u32, u8)> {
        self.stats.current_spec()
    }

    /// Return whether the sink is currently open.
    pub fn is_open(&self) -> bool {
        self.stats.is_open()
    }

    /// Return the total number of `open` calls.
    pub fn open_calls(&self) -> u64 {
        self.stats.open_calls()
    }

    /// Return the total number of `f32` samples passed to `write_f32`.
    pub fn written_samples(&self) -> u64 {
        self.stats.written_samples()
    }

    /// Return the total number of non-empty `write_f32` calls.
    pub fn write_calls(&self) -> u64 {
        self.stats.write_calls()
    }

    /// Return the total number of `flush` calls.
    pub fn flush_count(&self) -> u64 {
        self.stats.flush_count()
    }

    /// Return the total number of `drain` calls.
    pub fn drain_count(&self) -> u64 {
        self.stats.drain_count()
    }

    /// Return the total number of `close` transitions.
    pub fn close_count(&self) -> u64 {
        self.stats.close_count()
    }
}

impl AudioSink for NullSink {
    fn open(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        if sample_rate == 0 || channels == 0 {
            return Err(TunrError::Audio(format!(
                "invalid audio specification: {sample_rate} Hz, {channels} channels"
            )));
        }

        self.stats.open_calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.stats.lock_state();
        if state.is_open && state.current_spec == Some((sample_rate, channels)) {
            return Ok(());
        }

        state.current_spec = Some((sample_rate, channels));
        state.opened_specs.push((sample_rate, channels));
        state.is_open = true;
        self.stats.is_open.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
        if !self.stats.is_open() {
            return Err(TunrError::Audio(
                "cannot write samples: null sink is not open".to_string(),
            ));
        }
        if samples.is_empty() {
            return Ok(());
        }

        let sample_count = u64::try_from(samples.len()).unwrap_or(u64::MAX);
        self.stats
            .written_samples
            .fetch_add(sample_count, Ordering::SeqCst);
        self.stats.write_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.pacing {
            std::thread::sleep(delay);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.stats.flush_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        self.stats.drain_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn close(&mut self) {
        let mut state = self.stats.lock_state();
        self.stats.is_open.store(false, Ordering::SeqCst);
        if state.is_open {
            state.is_open = false;
            self.stats.close_count.fetch_add(1, Ordering::SeqCst);
        }
        state.current_spec = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_before_open_fails() {
        let mut sink = NullSink::new();
        let err = sink.write_f32(&[0.1, -0.1]).unwrap_err();
        assert!(matches!(err, TunrError::Audio(_)));
    }

    #[test]
    fn open_tracks_spec_changes_and_shared_counters() {
        let mut sink = NullSink::new();
        let stats = sink.stats();

        sink.open(44_100, 2).unwrap();
        // Same parameters should not record a duplicate reopen.
        sink.open(44_100, 2).unwrap();
        sink.write_f32(&[0.0; 128]).unwrap();
        sink.flush().unwrap();

        sink.open(48_000, 1).unwrap();
        sink.write_f32(&[0.25; 64]).unwrap();
        sink.drain().unwrap();
        sink.close();
        // Closing twice is a safe no-op.
        sink.close();

        assert_eq!(stats.opened_specs(), vec![(44_100, 2), (48_000, 1)]);
        assert_eq!(stats.open_calls(), 3);
        assert_eq!(stats.written_samples(), 192);
        assert_eq!(stats.write_calls(), 2);
        assert_eq!(stats.flush_count(), 1);
        assert_eq!(stats.drain_count(), 1);
        assert_eq!(stats.close_count(), 1);
        assert!(!stats.is_open());
        assert_eq!(stats.current_spec(), None);
    }

    #[test]
    fn invalid_spec_is_rejected() {
        let mut sink = NullSink::new();
        assert!(matches!(sink.open(0, 2), Err(TunrError::Audio(_))));
        assert!(matches!(sink.open(44_100, 0), Err(TunrError::Audio(_))));
    }

    #[test]
    fn pacing_delays_writes() {
        let mut sink = NullSink::with_pacing(Duration::from_millis(5));
        sink.open(44_100, 2).unwrap();
        let start = std::time::Instant::now();
        sink.write_f32(&[0.0; 16]).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(4));
    }

    #[test]
    fn poisoned_stats_mutex_does_not_panic() {
        let mut sink = NullSink::new();
        let stats = sink.stats();
        let stats_for_panic = Arc::clone(&stats);

        let handle = std::thread::spawn(move || {
            let _guard = stats_for_panic.lock_state();
            panic!("intentional mutex poison");
        });
        assert!(handle.join().is_err());

        sink.open(44_100, 2).unwrap();
        sink.write_f32(&[0.25; 8]).unwrap();
        assert_eq!(stats.current_spec(), Some((44_100, 2)));
        assert_eq!(stats.opened_specs(), vec![(44_100, 2)]);
        sink.close();
        sink.close();
        assert!(!stats.is_open());
        assert_eq!(stats.close_count(), 1);
        stats.reset();
        assert!(stats.opened_specs().is_empty());
    }
}
