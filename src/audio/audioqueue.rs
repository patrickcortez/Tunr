//! macOS CoreAudio AudioQueue output backend linking `AudioToolbox.framework`.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::audio::AudioSink;
use crate::error::{Result, TermusicError};

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const NUM_BUFFERS: usize = 4;
const TARGET_BUFFER_MS: u32 = 40;
const MIN_BUFFER_FRAMES: usize = 512;
const BUFFER_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DRAIN_WAIT: Duration = Duration::from_millis(1500);

type OSStatus = i32;
type Boolean = u8;
type AudioQueueRef = *mut c_void;
type AudioQueueBufferRef = *mut AudioQueueBuffer;
type AudioQueueOutputCallback =
    unsafe extern "C" fn(in_user_data: *mut c_void, in_aq: AudioQueueRef, in_buf: AudioQueueBufferRef);

#[allow(dead_code)]
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioStreamBasicDescription {
    m_sample_rate: f64,
    m_format_id: u32,
    m_format_flags: u32,
    m_bytes_per_packet: u32,
    m_frames_per_packet: u32,
    m_bytes_per_frame: u32,
    m_channels_per_frame: u32,
    m_bits_per_channel: u32,
    m_reserved: u32,
}

#[allow(dead_code)]
#[repr(C)]
struct AudioQueueBuffer {
    m_audio_data_bytes_capacity: u32,
    m_audio_data: *mut c_void,
    m_audio_data_byte_size: u32,
    m_user_data: *mut c_void,
    m_packet_description_capacity: u32,
    m_packet_descriptions: *const c_void,
    m_packet_description_count: u32,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        in_format: *const AudioStreamBasicDescription,
        in_callback_proc: AudioQueueOutputCallback,
        in_user_data: *mut c_void,
        in_callback_run_loop: *const c_void,
        in_callback_run_loop_mode: *const c_void,
        in_flags: u32,
        out_aq: *mut AudioQueueRef,
    ) -> OSStatus;

    fn AudioQueueAllocateBuffer(
        in_aq: AudioQueueRef,
        in_buffer_byte_size: u32,
        out_buffer: *mut AudioQueueBufferRef,
    ) -> OSStatus;

    fn AudioQueueEnqueueBuffer(
        in_aq: AudioQueueRef,
        in_buffer: AudioQueueBufferRef,
        in_num_packet_descs: u32,
        in_packet_descs: *const c_void,
    ) -> OSStatus;

    fn AudioQueueStart(in_aq: AudioQueueRef, in_start_time: *const c_void) -> OSStatus;
    fn AudioQueueStop(in_aq: AudioQueueRef, in_immediate: Boolean) -> OSStatus;
    fn AudioQueueReset(in_aq: AudioQueueRef) -> OSStatus;
    fn AudioQueueFlush(in_aq: AudioQueueRef) -> OSStatus;
    fn AudioQueueDispose(in_aq: AudioQueueRef, in_immediate: Boolean) -> OSStatus;
}

struct QueueInner {
    free_buffers: VecDeque<AudioQueueBufferRef>,
    enqueued_count: usize,
    disposing: bool,
}

unsafe impl Send for QueueInner {}

struct QueueState {
    inner: Mutex<QueueInner>,
    cv: Condvar,
}

struct ActiveQueue {
    aq: NonNull<c_void>,
    state: Arc<QueueState>,
    raw_state: *const QueueState,
    buffer_cap_samples: usize,
}

unsafe impl Send for ActiveQueue {}

unsafe extern "C" fn output_callback(
    in_user_data: *mut c_void,
    _in_aq: AudioQueueRef,
    in_buf: AudioQueueBufferRef,
) {
    if in_user_data.is_null() || in_buf.is_null() {
        return;
    }
    let state = unsafe { &*(in_user_data as *const QueueState) };
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.enqueued_count = inner.enqueued_count.saturating_sub(1);
    if !inner.disposing {
        inner.free_buffers.push_back(in_buf);
    }
    state.cv.notify_all();
}

fn build_asbd(sample_rate: u32, channels: u8) -> Result<AudioStreamBasicDescription> {
    if sample_rate == 0 {
        return Err(TermusicError::Audio("AudioQueue sample_rate must be > 0".to_string()));
    }
    if channels == 0 {
        return Err(TermusicError::Audio("AudioQueue channels must be > 0".to_string()));
    }
    let bytes_per_frame = u32::from(channels) * 4;
    Ok(AudioStreamBasicDescription {
        m_sample_rate: f64::from(sample_rate),
        m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
        m_format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
        m_bytes_per_packet: bytes_per_frame,
        m_frames_per_packet: 1,
        m_bytes_per_frame: bytes_per_frame,
        m_channels_per_frame: u32::from(channels),
        m_bits_per_channel: 32,
        m_reserved: 0,
    })
}

fn buffer_frames_for_rate(sample_rate: u32) -> usize {
    let target = (u64::from(sample_rate) * u64::from(TARGET_BUFFER_MS) / 1000) as usize;
    target.max(MIN_BUFFER_FRAMES)
}

fn check_osstatus(context: &str, status: OSStatus) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(TermusicError::Audio(format!("{context} failed with OSStatus {status}")))
    }
}

/// Blocking macOS CoreAudio AudioQueue PCM output sink implementing [`AudioSink`].
pub struct AudioQueueSink {
    stream: Option<ActiveQueue>,
    spec: Option<(u32, u8)>,
    staging: Vec<f32>,
    started: bool,
}

impl AudioQueueSink {
    pub fn new() -> Result<Self> {
        Ok(Self {
            stream: None,
            spec: None,
            staging: Vec::new(),
            started: false,
        })
    }

    fn open_queue(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        let asbd = build_asbd(sample_rate, channels)?;
        let buffer_frames = buffer_frames_for_rate(sample_rate);
        let buffer_cap_samples = buffer_frames * usize::from(channels);
        let buffer_byte_size = (buffer_cap_samples * std::mem::size_of::<f32>()) as u32;

        let state = Arc::new(QueueState {
            inner: Mutex::new(QueueInner {
                free_buffers: VecDeque::with_capacity(NUM_BUFFERS),
                enqueued_count: 0,
                disposing: false,
            }),
            cv: Condvar::new(),
        });
        let raw_state = Arc::into_raw(Arc::clone(&state));

        let mut aq_raw: AudioQueueRef = ptr::null_mut();
        let status = unsafe {
            AudioQueueNewOutput(
                &asbd,
                output_callback,
                raw_state as *mut c_void,
                ptr::null(),
                ptr::null(),
                0,
                &mut aq_raw,
            )
        };
        if status != 0 || aq_raw.is_null() {
            unsafe { drop(Arc::from_raw(raw_state)) };
            return Err(TermusicError::Audio(format!(
                "AudioQueueNewOutput failed with OSStatus {status}"
            )));
        }

        for _ in 0..NUM_BUFFERS {
            let mut buf: AudioQueueBufferRef = ptr::null_mut();
            let alloc_status = unsafe { AudioQueueAllocateBuffer(aq_raw, buffer_byte_size, &mut buf) };
            if alloc_status != 0 || buf.is_null() {
                {
                    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
                    inner.disposing = true;
                }
                let _ = unsafe { AudioQueueDispose(aq_raw, 1) };
                unsafe { drop(Arc::from_raw(raw_state)) };
                return Err(TermusicError::Audio(format!(
                    "AudioQueueAllocateBuffer failed with OSStatus {alloc_status}"
                )));
            }
            let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.free_buffers.push_back(buf);
        }

        self.stream = Some(ActiveQueue {
            aq: NonNull::new(aq_raw).expect("checked non-null"),
            state,
            raw_state,
            buffer_cap_samples,
        });
        self.spec = Some((sample_rate, channels));
        self.staging.clear();
        self.started = false;
        Ok(())
    }

    fn enqueue_slice(&mut self, samples: &[f32]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| TermusicError::Audio("AudioQueueSink stream not open".to_string()))?;

        let aq = stream.aq.as_ptr();
        let byte_len = samples.len() * std::mem::size_of::<f32>();

        let buf = {
            let mut inner = stream.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            let deadline = Instant::now() + BUFFER_WAIT_TIMEOUT;
            while inner.free_buffers.is_empty() && !inner.disposing {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(TermusicError::Audio(
                        "AudioQueue timed out waiting for a free output buffer".to_string(),
                    ));
                }
                let (guard, timeout_res) = stream.state.cv.wait_timeout(inner, remaining).unwrap_or_else(|e| e.into_inner());
                inner = guard;
                if timeout_res.timed_out() && inner.free_buffers.is_empty() {
                    return Err(TermusicError::Audio(
                        "AudioQueue timed out waiting for a free output buffer".to_string(),
                    ));
                }
            }
            if inner.disposing {
                return Err(TermusicError::Audio("AudioQueue is disposing".to_string()));
            }
            let buf = inner.free_buffers.pop_front().expect("checked non-empty");
            inner.enqueued_count += 1;
            buf
        };

        // Never hold `state.inner` across CoreAudio FFI calls!
        unsafe {
            let buf_ref = &mut *buf;
            if (buf_ref.m_audio_data_bytes_capacity as usize) < byte_len || buf_ref.m_audio_data.is_null() {
                let mut inner = stream.state.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.enqueued_count = inner.enqueued_count.saturating_sub(1);
                inner.free_buffers.push_back(buf);
                return Err(TermusicError::Audio(format!(
                    "AudioQueue buffer capacity {} too small for {byte_len} bytes",
                    buf_ref.m_audio_data_bytes_capacity
                )));
            }
            ptr::copy_nonoverlapping(samples.as_ptr(), buf_ref.m_audio_data.cast::<f32>(), samples.len());
            buf_ref.m_audio_data_byte_size = byte_len as u32;
        }

        let enq_status = unsafe { AudioQueueEnqueueBuffer(aq, buf, 0, ptr::null()) };
        if enq_status != 0 {
            let mut inner = stream.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.enqueued_count = inner.enqueued_count.saturating_sub(1);
            if !inner.disposing {
                inner.free_buffers.push_back(buf);
            }
            return check_osstatus("AudioQueueEnqueueBuffer", enq_status);
        }

        if !self.started {
            let start_status = unsafe { AudioQueueStart(aq, ptr::null()) };
            check_osstatus("AudioQueueStart", start_status)?;
            self.started = true;
        }

        Ok(())
    }

    fn flush_staging_partial(&mut self) -> Result<()> {
        if self.staging.is_empty() {
            return Ok(());
        }
        let Some(cap) = self.stream.as_ref().map(|s| s.buffer_cap_samples) else {
            self.staging.clear();
            return Ok(());
        };
        let staged = std::mem::take(&mut self.staging);
        for chunk in staged.chunks(cap) {
            self.enqueue_slice(chunk)?;
        }
        Ok(())
    }
}

impl AudioSink for AudioQueueSink {
    fn open(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        if sample_rate == 0 {
            return Err(TermusicError::Audio("AudioQueue sample_rate must be > 0".to_string()));
        }
        if channels == 0 {
            return Err(TermusicError::Audio("AudioQueue channels must be > 0".to_string()));
        }

        if self.stream.is_some() && self.spec == Some((sample_rate, channels)) {
            return Ok(());
        }

        self.close();
        self.open_queue(sample_rate, channels)
    }

    fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
        let (_, channels) = self
            .spec
            .ok_or_else(|| TermusicError::Audio("AudioQueueSink::write_f32 called before open()".to_string()))?;
        let ch = usize::from(channels);
        if samples.len() % ch != 0 {
            return Err(TermusicError::Audio(format!(
                "PCM sample slice length {} is not divisible by {ch} channels",
                samples.len()
            )));
        }
        if samples.is_empty() {
            return Ok(());
        }

        let cap = self
            .stream
            .as_ref()
            .map(|s| s.buffer_cap_samples)
            .ok_or_else(|| TermusicError::Audio("AudioQueueSink stream not open".to_string()))?;

        // Fast path when staging is empty and input already spans whole coalesced chunks.
        let mut rest = samples;
        if !self.staging.is_empty() {
            let need = cap.saturating_sub(self.staging.len());
            if rest.len() < need {
                self.staging.extend_from_slice(rest);
                return Ok(());
            }
            self.staging.extend_from_slice(&rest[..need]);
            rest = &rest[need..];
            let chunk = std::mem::take(&mut self.staging);
            self.enqueue_slice(&chunk)?;
        }

        while rest.len() >= cap {
            self.enqueue_slice(&rest[..cap])?;
            rest = &rest[cap..];
        }
        if !rest.is_empty() {
            self.staging.extend_from_slice(rest);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.staging.clear();
        let Some(stream) = self.stream.as_ref() else {
            self.started = false;
            return Ok(());
        };
        let aq = stream.aq.as_ptr();
        // Do not lock `stream.state.inner`: AudioQueueStop(immediate=1)/Reset invokes callbacks synchronously.
        let stop_rc = unsafe { AudioQueueStop(aq, 1) };
        let reset_rc = unsafe { AudioQueueReset(aq) };
        self.started = false;
        check_osstatus("AudioQueueStop", stop_rc)?;
        check_osstatus("AudioQueueReset", reset_rc)?;
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        if self.stream.is_none() {
            self.staging.clear();
            self.started = false;
            return Ok(());
        }
        self.flush_staging_partial()?;

        let Some(stream) = self.stream.as_ref() else {
            return Ok(());
        };
        let in_flight = {
            let inner = stream.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.enqueued_count
        };
        if !self.started && in_flight == 0 {
            return Ok(());
        }

        let aq = stream.aq.as_ptr();
        check_osstatus("AudioQueueFlush", unsafe { AudioQueueFlush(aq) })?;

        let deadline = Instant::now() + MAX_DRAIN_WAIT;
        {
            let mut inner = stream.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            while inner.enqueued_count > 0 && !inner.disposing {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (guard, timeout_res) = stream
                    .state
                    .cv
                    .wait_timeout(inner, remaining)
                    .unwrap_or_else(|e| e.into_inner());
                inner = guard;
                if timeout_res.timed_out() {
                    break;
                }
            }
        }

        let _ = unsafe { AudioQueueStop(aq, 1) };
        self.started = false;
        Ok(())
    }

    fn close(&mut self) {
        self.staging.clear();
        self.spec = None;
        self.started = false;

        if let Some(stream) = self.stream.take() {
            {
                let mut inner = stream.state.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.disposing = true;
                stream.state.cv.notify_all();
            }
            // AudioQueueDispose(in_immediate=1) tears down callbacks synchronously;
            // only reclaim raw_state after Dispose returns!
            let _ = unsafe { AudioQueueDispose(stream.aq.as_ptr(), 1) };
            unsafe { drop(Arc::from_raw(stream.raw_state)) };
            drop(stream.state);
        }
    }
}

impl Drop for AudioQueueSink {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asbd_and_buffer_abi_layout() {
        assert_eq!(std::mem::size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(K_AUDIO_FORMAT_LINEAR_PCM, 0x6C70_636D);
        assert_eq!(K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED, 9);

        let asbd = build_asbd(44_100, 2).unwrap();
        assert_eq!(asbd.m_sample_rate, 44_100.0);
        assert_eq!(asbd.m_format_id, K_AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(asbd.m_format_flags, 9);
        assert_eq!(asbd.m_bytes_per_packet, 8);
        assert_eq!(asbd.m_frames_per_packet, 1);
        assert_eq!(asbd.m_bytes_per_frame, 8);
        assert_eq!(asbd.m_channels_per_frame, 2);
        assert_eq!(asbd.m_bits_per_channel, 32);

        assert_eq!(buffer_frames_for_rate(48_000), 1920);
        assert_eq!(buffer_frames_for_rate(8_000), MIN_BUFFER_FRAMES);
        assert!(build_asbd(0, 2).is_err());
        assert!(build_asbd(44_100, 0).is_err());
    }

    #[test]
    #[ignore = "requires macOS AudioToolbox audio device"]
    fn live_audioqueue_open_write_flush_drain_close() {
        let mut sink = AudioQueueSink::new().unwrap();
        sink.flush().unwrap();
        sink.drain().unwrap();
        sink.open(44_100, 2).unwrap();
        sink.write_f32(&vec![0.0; 2048]).unwrap();
        sink.flush().unwrap();
        sink.open(44_100, 2).unwrap();
        sink.drain().unwrap();
        sink.write_f32(&vec![0.0; 2048]).unwrap();
        sink.drain().unwrap();
        sink.close();
        sink.close();
    }
}
