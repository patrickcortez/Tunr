//! Android AAudio output backend loaded dynamically from `libaaudio.so`.
//!
//! Designed for Termux (`aarch64-linux-android` / API 26+) without requiring
//! NDK headers at compile time or a running Termux PulseAudio daemon.

use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomPinned;
use std::ptr::{NonNull, null_mut};
use std::time::{Duration, Instant};

use libloading::Library;

use crate::audio::AudioSink;
use crate::audio::resample::PcmAdapter;
use crate::error::{Result, TunrError};

const AAUDIO_OK: i32 = 0;
const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
const AAUDIO_FORMAT_PCM_I16: i32 = 1;
const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;
const AAUDIO_SHARING_MODE_SHARED: i32 = 1;
const AAUDIO_PERFORMANCE_MODE_NONE: i32 = 10;
const AAUDIO_USAGE_MEDIA: i32 = 1;
const AAUDIO_ERROR_DISCONNECTED: i32 = -899;

const AAUDIO_STREAM_STATE_OPEN: i32 = 2;
const AAUDIO_STREAM_STATE_STARTING: i32 = 3;
const AAUDIO_STREAM_STATE_STARTED: i32 = 4;
const AAUDIO_STREAM_STATE_PAUSING: i32 = 5;
const AAUDIO_STREAM_STATE_PAUSED: i32 = 6;
const AAUDIO_STREAM_STATE_FLUSHING: i32 = 7;
const AAUDIO_STREAM_STATE_FLUSHED: i32 = 8;
const AAUDIO_STREAM_STATE_STOPPING: i32 = 9;
const AAUDIO_STREAM_STATE_STOPPED: i32 = 10;
const AAUDIO_STREAM_STATE_CLOSING: i32 = 11;
const AAUDIO_STREAM_STATE_CLOSED: i32 = 12;
const AAUDIO_STREAM_STATE_DISCONNECTED: i32 = 13;

const STATE_WAIT_NS: i64 = 250_000_000; // 250 ms
const WRITE_TIMEOUT_NS: i64 = 1_500_000_000; // 1.5 s
const MAX_WRITE_STALL: Duration = Duration::from_secs(3);
const MAX_DRAIN_WAIT: Duration = Duration::from_millis(1500);
const MAX_DISCONNECT_RETRIES: usize = 2;

#[repr(C)]
struct AAudioStreamBuilder {
    _data: [u8; 0],
    _marker: PhantomPinned,
}

#[repr(C)]
struct AAudioStream {
    _data: [u8; 0],
    _marker: PhantomPinned,
}

type CreateStreamBuilderFn = unsafe extern "C" fn(*mut *mut AAudioStreamBuilder) -> i32;
type DeleteStreamBuilderFn = unsafe extern "C" fn(*mut AAudioStreamBuilder) -> i32;
type SetIntBuilderFn = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type OpenStreamFn = unsafe extern "C" fn(*mut AAudioStreamBuilder, *mut *mut AAudioStream) -> i32;
type StreamResultFn = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type StreamGetI32Fn = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type StreamGetI64Fn = unsafe extern "C" fn(*mut AAudioStream) -> i64;
type WaitForStateChangeFn = unsafe extern "C" fn(*mut AAudioStream, i32, *mut i32, i64) -> i32;
type WriteFn = unsafe extern "C" fn(*mut AAudioStream, *const c_void, i32, i64) -> i32;
type ConvertResultToTextFn = unsafe extern "C" fn(i32) -> *const c_char;

struct AAudioApi {
    _lib: Library,
    create_stream_builder: CreateStreamBuilderFn,
    delete_stream_builder: DeleteStreamBuilderFn,
    set_direction: SetIntBuilderFn,
    set_sample_rate: SetIntBuilderFn,
    set_channel_count: SetIntBuilderFn,
    set_format: SetIntBuilderFn,
    set_sharing_mode: SetIntBuilderFn,
    set_performance_mode: SetIntBuilderFn,
    set_usage: Option<SetIntBuilderFn>,
    open_stream: OpenStreamFn,
    close_stream: StreamResultFn,
    request_start: StreamResultFn,
    request_pause: StreamResultFn,
    request_flush: StreamResultFn,
    request_stop: StreamResultFn,
    get_state: StreamGetI32Fn,
    wait_for_state_change: WaitForStateChangeFn,
    write: WriteFn,
    get_sample_rate: StreamGetI32Fn,
    get_channel_count: StreamGetI32Fn,
    get_format: StreamGetI32Fn,
    get_frames_read: StreamGetI64Fn,
    get_frames_written: StreamGetI64Fn,
    convert_result_to_text: Option<ConvertResultToTextFn>,
}

unsafe impl Send for AAudioApi {}

unsafe fn load_sym<T: Copy>(lib: &Library, name: &[u8], label: &str) -> Result<T> {
    let sym = unsafe { lib.get::<T>(name) }
        .map_err(|err| TunrError::Audio(format!("missing symbol '{label}' in libaaudio.so: {err}")))?;
    Ok(*sym)
}

impl AAudioApi {
    fn load() -> Result<Self> {
        let lib = unsafe { Library::new("libaaudio.so") }.map_err(|err| {
            TunrError::Audio(format!("failed to load libaaudio.so (requires Android 8.0+ / API 26+): {err}"))
        })?;

        let create_stream_builder =
            unsafe { load_sym::<CreateStreamBuilderFn>(&lib, b"AAudio_createStreamBuilder\0", "AAudio_createStreamBuilder") }?;
        let delete_stream_builder =
            unsafe { load_sym::<DeleteStreamBuilderFn>(&lib, b"AAudioStreamBuilder_delete\0", "AAudioStreamBuilder_delete") }?;
        let set_direction = unsafe {
            load_sym::<SetIntBuilderFn>(&lib, b"AAudioStreamBuilder_setDirection\0", "AAudioStreamBuilder_setDirection")
        }?;
        let set_sample_rate = unsafe {
            load_sym::<SetIntBuilderFn>(&lib, b"AAudioStreamBuilder_setSampleRate\0", "AAudioStreamBuilder_setSampleRate")
        }?;
        let set_channel_count = unsafe {
            load_sym::<SetIntBuilderFn>(
                &lib,
                b"AAudioStreamBuilder_setChannelCount\0",
                "AAudioStreamBuilder_setChannelCount",
            )
        }?;
        let set_format =
            unsafe { load_sym::<SetIntBuilderFn>(&lib, b"AAudioStreamBuilder_setFormat\0", "AAudioStreamBuilder_setFormat") }?;
        let set_sharing_mode = unsafe {
            load_sym::<SetIntBuilderFn>(
                &lib,
                b"AAudioStreamBuilder_setSharingMode\0",
                "AAudioStreamBuilder_setSharingMode",
            )
        }?;
        let set_performance_mode = unsafe {
            load_sym::<SetIntBuilderFn>(
                &lib,
                b"AAudioStreamBuilder_setPerformanceMode\0",
                "AAudioStreamBuilder_setPerformanceMode",
            )
        }?;
        let set_usage = unsafe { lib.get::<SetIntBuilderFn>(b"AAudioStreamBuilder_setUsage\0") }
            .ok()
            .map(|s| *s);
        let open_stream =
            unsafe { load_sym::<OpenStreamFn>(&lib, b"AAudioStreamBuilder_openStream\0", "AAudioStreamBuilder_openStream") }?;
        let close_stream = unsafe { load_sym::<StreamResultFn>(&lib, b"AAudioStream_close\0", "AAudioStream_close") }?;
        let request_start =
            unsafe { load_sym::<StreamResultFn>(&lib, b"AAudioStream_requestStart\0", "AAudioStream_requestStart") }?;
        let request_pause =
            unsafe { load_sym::<StreamResultFn>(&lib, b"AAudioStream_requestPause\0", "AAudioStream_requestPause") }?;
        let request_flush =
            unsafe { load_sym::<StreamResultFn>(&lib, b"AAudioStream_requestFlush\0", "AAudioStream_requestFlush") }?;
        let request_stop =
            unsafe { load_sym::<StreamResultFn>(&lib, b"AAudioStream_requestStop\0", "AAudioStream_requestStop") }?;
        let get_state =
            unsafe { load_sym::<StreamGetI32Fn>(&lib, b"AAudioStream_getState\0", "AAudioStream_getState") }?;
        let wait_for_state_change = unsafe {
            load_sym::<WaitForStateChangeFn>(&lib, b"AAudioStream_waitForStateChange\0", "AAudioStream_waitForStateChange")
        }?;
        let write = unsafe { load_sym::<WriteFn>(&lib, b"AAudioStream_write\0", "AAudioStream_write") }?;
        let get_sample_rate =
            unsafe { load_sym::<StreamGetI32Fn>(&lib, b"AAudioStream_getSampleRate\0", "AAudioStream_getSampleRate") }?;
        let get_channel_count =
            unsafe { load_sym::<StreamGetI32Fn>(&lib, b"AAudioStream_getChannelCount\0", "AAudioStream_getChannelCount") }?;
        let get_format =
            unsafe { load_sym::<StreamGetI32Fn>(&lib, b"AAudioStream_getFormat\0", "AAudioStream_getFormat") }?;
        let get_frames_read =
            unsafe { load_sym::<StreamGetI64Fn>(&lib, b"AAudioStream_getFramesRead\0", "AAudioStream_getFramesRead") }?;
        let get_frames_written =
            unsafe { load_sym::<StreamGetI64Fn>(&lib, b"AAudioStream_getFramesWritten\0", "AAudioStream_getFramesWritten") }?;
        let convert_result_to_text = unsafe { lib.get::<ConvertResultToTextFn>(b"AAudio_convertResultToText\0") }
            .ok()
            .map(|s| *s);

        Ok(Self {
            _lib: lib,
            create_stream_builder,
            delete_stream_builder,
            set_direction,
            set_sample_rate,
            set_channel_count,
            set_format,
            set_sharing_mode,
            set_performance_mode,
            set_usage,
            open_stream,
            close_stream,
            request_start,
            request_pause,
            request_flush,
            request_stop,
            get_state,
            wait_for_state_change,
            write,
            get_sample_rate,
            get_channel_count,
            get_format,
            get_frames_read,
            get_frames_written,
            convert_result_to_text,
        })
    }

    fn result_name(&self, code: i32) -> String {
        if let Some(conv) = self.convert_result_to_text {
            let ptr = unsafe { conv(code) };
            if !ptr.is_null() {
                return unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
            }
        }
        format!("code {code}")
    }

    fn error_for(&self, context: &str, code: i32) -> TunrError {
        TunrError::Audio(format!("{context}: {} ({code})", self.result_name(code)))
    }
}

struct BuilderGuard<'a> {
    api: &'a AAudioApi,
    ptr: NonNull<AAudioStreamBuilder>,
}

impl Drop for BuilderGuard<'_> {
    fn drop(&mut self) {
        let _ = unsafe { (self.api.delete_stream_builder)(self.ptr.as_ptr()) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceSampleFormat {
    F32,
    I16,
}

/// Blocking Android AAudio PCM output sink implementing [`AudioSink`].
pub struct AAudioSink {
    api: AAudioApi,
    stream: Option<NonNull<AAudioStream>>,
    requested_spec: Option<(u32, u8)>,
    device_format: DeviceSampleFormat,
    adapter: Option<PcmAdapter>,
    started: bool,
    scratch_f32: Vec<f32>,
    scratch_i16: Vec<i16>,
}

unsafe impl Send for AAudioSink {}

impl AAudioSink {
    /// Loads `libaaudio.so` and resolves required AAudio symbols without opening a stream yet.
    pub fn new() -> Result<Self> {
        let api = AAudioApi::load()?;
        Ok(Self {
            api,
            stream: None,
            requested_spec: None,
            device_format: DeviceSampleFormat::F32,
            adapter: None,
            started: false,
            scratch_f32: Vec::new(),
            scratch_i16: Vec::new(),
        })
    }

    fn open_stream_for(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        let mut raw_builder = null_mut();
        let rc = unsafe { (self.api.create_stream_builder)(&mut raw_builder) };
        if rc != AAUDIO_OK || raw_builder.is_null() {
            return Err(self.api.error_for("AAudio_createStreamBuilder failed", rc));
        }
        let builder = BuilderGuard {
            api: &self.api,
            ptr: NonNull::new(raw_builder).expect("checked non-null"),
        };

        unsafe {
            (self.api.set_direction)(builder.ptr.as_ptr(), AAUDIO_DIRECTION_OUTPUT);
            (self.api.set_sample_rate)(builder.ptr.as_ptr(), sample_rate as i32);
            (self.api.set_channel_count)(builder.ptr.as_ptr(), i32::from(channels));
            (self.api.set_format)(builder.ptr.as_ptr(), AAUDIO_FORMAT_PCM_FLOAT);
            (self.api.set_sharing_mode)(builder.ptr.as_ptr(), AAUDIO_SHARING_MODE_SHARED);
            (self.api.set_performance_mode)(builder.ptr.as_ptr(), AAUDIO_PERFORMANCE_MODE_NONE);
            if let Some(set_usage) = self.api.set_usage {
                set_usage(builder.ptr.as_ptr(), AAUDIO_USAGE_MEDIA);
            }
        }

        let mut raw_stream = null_mut();
        let rc = unsafe { (self.api.open_stream)(builder.ptr.as_ptr(), &mut raw_stream) };
        if rc != AAUDIO_OK || raw_stream.is_null() {
            return Err(self.api.error_for("AAudioStreamBuilder_openStream failed", rc));
        }

        let actual_rate = unsafe { (self.api.get_sample_rate)(raw_stream) };
        let actual_channels = unsafe { (self.api.get_channel_count)(raw_stream) };
        let actual_format = unsafe { (self.api.get_format)(raw_stream) };

        if actual_rate <= 0 || actual_channels <= 0 {
            let _ = unsafe { (self.api.close_stream)(raw_stream) };
            return Err(TunrError::Audio(format!(
                "AAudio negotiated invalid stream format: rate={actual_rate}, channels={actual_channels}"
            )));
        }

        let device_format = match actual_format {
            AAUDIO_FORMAT_PCM_FLOAT => DeviceSampleFormat::F32,
            AAUDIO_FORMAT_PCM_I16 => DeviceSampleFormat::I16,
            other => {
                let _ = unsafe { (self.api.close_stream)(raw_stream) };
                return Err(TunrError::Audio(format!(
                    "AAudio negotiated unsupported PCM format {other}"
                )));
            }
        };

        let adapter = match PcmAdapter::new(
            sample_rate,
            usize::from(channels),
            actual_rate as u32,
            actual_channels as usize,
        ) {
            Ok(a) => a,
            Err(err) => {
                let _ = unsafe { (self.api.close_stream)(raw_stream) };
                return Err(err);
            }
        };

        self.stream = NonNull::new(raw_stream);
        self.requested_spec = Some((sample_rate, channels));
        self.device_format = device_format;
        self.adapter = Some(adapter);
        self.started = false;
        Ok(())
    }

    fn is_transitional(state: i32) -> bool {
        matches!(
            state,
            AAUDIO_STREAM_STATE_STARTING
                | AAUDIO_STREAM_STATE_PAUSING
                | AAUDIO_STREAM_STATE_FLUSHING
                | AAUDIO_STREAM_STATE_STOPPING
        )
    }

    fn is_dead_state(state: i32) -> bool {
        matches!(
            state,
            AAUDIO_STREAM_STATE_CLOSING | AAUDIO_STREAM_STATE_CLOSED | AAUDIO_STREAM_STATE_DISCONNECTED
        )
    }

    fn settle_state(&self, stream: *mut AAudioStream) -> i32 {
        let mut state = unsafe { (self.api.get_state)(stream) };
        for _ in 0..4 {
            if !Self::is_transitional(state) {
                break;
            }
            let mut next = state;
            let rc = unsafe { (self.api.wait_for_state_change)(stream, state, &mut next, STATE_WAIT_NS) };
            if rc != AAUDIO_OK {
                state = unsafe { (self.api.get_state)(stream) };
                break;
            }
            state = next;
        }
        state
    }

    fn ensure_started(&mut self) -> std::result::Result<(), AAudioWriteErr> {
        let stream = self
            .stream
            .ok_or_else(|| AAudioWriteErr::Fatal(TunrError::Audio("AAudioSink is not open".to_string())))?
            .as_ptr();
        if self.started {
            return Ok(());
        }

        let state = self.settle_state(stream);
        if Self::is_dead_state(state) {
            return Err(AAudioWriteErr::Disconnected);
        }
        if state == AAUDIO_STREAM_STATE_STARTED {
            self.started = true;
            return Ok(());
        }
        if matches!(
            state,
            AAUDIO_STREAM_STATE_OPEN
                | AAUDIO_STREAM_STATE_PAUSED
                | AAUDIO_STREAM_STATE_FLUSHED
                | AAUDIO_STREAM_STATE_STOPPED
        ) {
            let rc = unsafe { (self.api.request_start)(stream) };
            if rc == AAUDIO_ERROR_DISCONNECTED {
                return Err(AAudioWriteErr::Disconnected);
            }
            if rc != AAUDIO_OK {
                return Err(AAudioWriteErr::Fatal(
                    self.api.error_for("AAudioStream_requestStart failed", rc),
                ));
            }
            let after = self.settle_state(stream);
            if Self::is_dead_state(after) {
                return Err(AAudioWriteErr::Disconnected);
            }
            self.started = true;
            return Ok(());
        }

        Err(AAudioWriteErr::Fatal(TunrError::Audio(format!(
            "unexpected AAudio stream state {state} before start"
        ))))
    }

    fn write_device_frames(&mut self, interleaved_f32: &[f32]) -> std::result::Result<usize, PartialDisconnect> {
        let dst_channels = self
            .adapter
            .as_ref()
            .map(PcmAdapter::dst_channels)
            .unwrap_or(2);
        let total_frames = interleaved_f32.len() / dst_channels;
        if total_frames == 0 {
            return Ok(0);
        }

        if self.device_format == DeviceSampleFormat::I16 {
            convert_f32_to_i16(interleaved_f32, &mut self.scratch_i16);
        }

        let mut written_frames = 0usize;
        let mut stall_start: Option<Instant> = None;

        while written_frames < total_frames {
            if let Err(err) = self.ensure_started() {
                return match err {
                    AAudioWriteErr::Disconnected => Err(PartialDisconnect {
                        written_dst_frames: written_frames,
                        fatal: None,
                    }),
                    AAudioWriteErr::Fatal(e) => Err(PartialDisconnect {
                        written_dst_frames: written_frames,
                        fatal: Some(e),
                    }),
                };
            }

            let stream = self.stream.expect("checked open in ensure_started").as_ptr();
            let remaining = total_frames - written_frames;
            let chunk_frames = remaining.min(i32::MAX as usize) as i32;
            let sample_offset = written_frames * dst_channels;

            let rc = match self.device_format {
                DeviceSampleFormat::F32 => unsafe {
                    (self.api.write)(
                        stream,
                        interleaved_f32[sample_offset..].as_ptr() as *const c_void,
                        chunk_frames,
                        WRITE_TIMEOUT_NS,
                    )
                },
                DeviceSampleFormat::I16 => unsafe {
                    (self.api.write)(
                        stream,
                        self.scratch_i16[sample_offset..].as_ptr() as *const c_void,
                        chunk_frames,
                        WRITE_TIMEOUT_NS,
                    )
                },
            };

            if rc > 0 {
                written_frames += rc as usize;
                stall_start = None;
                continue;
            }

            let state = unsafe { (self.api.get_state)(stream) };
            if rc == AAUDIO_ERROR_DISCONNECTED || Self::is_dead_state(state) {
                return Err(PartialDisconnect {
                    written_dst_frames: written_frames,
                    fatal: None,
                });
            }

            let name = self.api.result_name(rc);
            if rc == 0 || name == "AAUDIO_ERROR_TIMEOUT" || name == "AAUDIO_ERROR_WOULD_BLOCK" {
                let start = stall_start.get_or_insert_with(Instant::now);
                if start.elapsed() >= MAX_WRITE_STALL {
                    return Err(PartialDisconnect {
                        written_dst_frames: written_frames,
                        fatal: Some(TunrError::Audio(format!(
                            "AAudioStream_write stalled without progress ({name})"
                        ))),
                    });
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }

            return Err(PartialDisconnect {
                written_dst_frames: written_frames,
                fatal: Some(self.api.error_for("AAudioStream_write failed", rc)),
            });
        }

        Ok(written_frames)
    }

    fn close_raw_stream_only(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = unsafe { (self.api.close_stream)(stream.as_ptr()) };
        }
        self.started = false;
    }
}

enum AAudioWriteErr {
    Disconnected,
    Fatal(TunrError),
}

struct PartialDisconnect {
    written_dst_frames: usize,
    fatal: Option<TunrError>,
}

fn convert_f32_to_i16(src: &[f32], dst: &mut Vec<i16>) {
    dst.clear();
    dst.reserve(src.len());
    for &sample in src {
        let clamped = if sample.is_nan() { 0.0 } else { sample.clamp(-1.0, 1.0) };
        dst.push((clamped * 32767.0).round() as i16);
    }
}

fn consumed_source_frames(
    written_dst_frames: usize,
    src_rate: u32,
    dst_rate: u32,
    remaining_src_frames: usize,
) -> usize {
    if written_dst_frames == 0 || remaining_src_frames == 0 {
        return 0;
    }
    if src_rate == dst_rate {
        return written_dst_frames.min(remaining_src_frames);
    }
    let scaled = ((written_dst_frames as u128) * u128::from(src_rate)) / u128::from(dst_rate);
    (scaled as usize).min(remaining_src_frames)
}

impl AudioSink for AAudioSink {
    fn open(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        if sample_rate == 0 {
            return Err(TunrError::Audio("AAudio sample_rate must be > 0".to_string()));
        }
        if channels == 0 {
            return Err(TunrError::Audio("AAudio channels must be > 0".to_string()));
        }

        if self.stream.is_some() && self.requested_spec == Some((sample_rate, channels)) {
            return Ok(());
        }

        self.close();
        self.open_stream_for(sample_rate, channels)
    }

    fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
        let (req_rate, req_channels) = self
            .requested_spec
            .ok_or_else(|| TunrError::Audio("AAudioSink::write_f32 called before open()".to_string()))?;
        let req_ch = usize::from(req_channels);
        if samples.len() % req_ch != 0 {
            return Err(TunrError::Audio(format!(
                "PCM sample slice length {} is not divisible by {req_ch} channels",
                samples.len()
            )));
        }
        if samples.is_empty() {
            return Ok(());
        }

        let mut src_offset_samples = 0usize;
        let mut disconnect_retries = 0usize;

        while src_offset_samples < samples.len() {
            let tail = &samples[src_offset_samples..];
            let (is_passthrough, dst_rate) = {
                let adapter = self
                    .adapter
                    .as_mut()
                    .ok_or_else(|| TunrError::Audio("AAudioSink missing PCM adapter".to_string()))?;
                let pt = adapter.is_passthrough();
                let dr = adapter.dst_rate();
                if !pt {
                    adapter.process_into(tail, &mut self.scratch_f32)?;
                }
                (pt, dr)
            };

            let write_res = if is_passthrough {
                self.write_device_frames(tail)
            } else {
                let buf = std::mem::take(&mut self.scratch_f32);
                let res = self.write_device_frames(&buf);
                self.scratch_f32 = buf;
                res
            };

            match write_res {
                Ok(_) => break,
                Err(PartialDisconnect { fatal: Some(err), .. }) => return Err(err),
                Err(PartialDisconnect {
                    written_dst_frames,
                    fatal: None,
                }) => {
                    disconnect_retries += 1;
                    if disconnect_retries > MAX_DISCONNECT_RETRIES {
                        return Err(TunrError::Audio(
                            "AAudio device stayed disconnected after reopen attempts".to_string(),
                        ));
                    }
                    let remaining_src_frames = tail.len() / req_ch;
                    let consumed_frames =
                        consumed_source_frames(written_dst_frames, req_rate, dst_rate, remaining_src_frames);
                    src_offset_samples += consumed_frames * req_ch;

                    // Do not requestPause/requestFlush on a disconnected handle: close & reopen cleanly.
                    self.close_raw_stream_only();
                    self.open_stream_for(req_rate, req_channels)?;
                }
            }
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let Some(stream_nn) = self.stream else {
            return Ok(());
        };
        let stream = stream_nn.as_ptr();
        let mut state = self.settle_state(stream);
        if Self::is_dead_state(state) {
            self.close();
            return Ok(());
        }

        if state == AAUDIO_STREAM_STATE_STARTED {
            let rc = unsafe { (self.api.request_pause)(stream) };
            if rc == AAUDIO_ERROR_DISCONNECTED {
                self.close();
                return Ok(());
            }
            if rc != AAUDIO_OK {
                return Err(self.api.error_for("AAudioStream_requestPause failed", rc));
            }
            state = self.settle_state(stream);
            if Self::is_dead_state(state) {
                self.close();
                return Ok(());
            }
        }

        if matches!(state, AAUDIO_STREAM_STATE_PAUSED | AAUDIO_STREAM_STATE_STOPPED) {
            let rc = unsafe { (self.api.request_flush)(stream) };
            if rc == AAUDIO_ERROR_DISCONNECTED {
                self.close();
                return Ok(());
            }
            if rc != AAUDIO_OK {
                return Err(self.api.error_for("AAudioStream_requestFlush failed", rc));
            }
            let after = self.settle_state(stream);
            if Self::is_dead_state(after) {
                self.close();
                return Ok(());
            }
        }

        self.started = false;
        if let Some(adapter) = self.adapter.as_mut() {
            adapter.reset();
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        if self.stream.is_none() || !self.started {
            return Ok(());
        }

        if let Some(adapter) = self.adapter.as_mut()
            && !adapter.is_passthrough()
        {
            adapter.flush_tail(&mut self.scratch_f32);
            if !self.scratch_f32.is_empty() {
                let tail = std::mem::take(&mut self.scratch_f32);
                let _ = self.write_device_frames(&tail);
                self.scratch_f32 = tail;
            }
        }

        let Some(stream_nn) = self.stream else {
            self.started = false;
            return Ok(());
        };
        let stream = stream_nn.as_ptr();

        let target_written = unsafe { (self.api.get_frames_written)(stream) }.max(0);
        let dst_rate = self
            .adapter
            .as_ref()
            .map(PcmAdapter::dst_rate)
            .unwrap_or(48_000)
            .max(1);
        let current_read = unsafe { (self.api.get_frames_read)(stream) }.max(0);
        let pending_frames = target_written.saturating_sub(current_read) as u64;
        let est_ms = ((pending_frames * 1000) / u64::from(dst_rate)) + 150;
        let deadline = Instant::now() + Duration::from_millis(est_ms.clamp(100, MAX_DRAIN_WAIT.as_millis() as u64));

        while Instant::now() < deadline {
            let state = unsafe { (self.api.get_state)(stream) };
            if Self::is_dead_state(state) {
                self.close();
                return Ok(());
            }
            let read = unsafe { (self.api.get_frames_read)(stream) }.max(0);
            if read >= target_written {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let _ = unsafe { (self.api.request_stop)(stream) };
        let _ = self.settle_state(stream);
        self.started = false;
        Ok(())
    }

    fn close(&mut self) {
        self.close_raw_stream_only();
        self.requested_spec = None;
        self.adapter = None;
    }
}

impl Drop for AAudioSink {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aaudio_state_and_error_constants_match_ndk_abi() {
        assert_eq!(AAUDIO_OK, 0);
        assert_eq!(AAUDIO_DIRECTION_OUTPUT, 0);
        assert_eq!(AAUDIO_FORMAT_PCM_I16, 1);
        assert_eq!(AAUDIO_FORMAT_PCM_FLOAT, 2);
        assert_eq!(AAUDIO_SHARING_MODE_SHARED, 1);
        assert_eq!(AAUDIO_PERFORMANCE_MODE_NONE, 10);
        assert_eq!(AAUDIO_USAGE_MEDIA, 1);
        assert_eq!(AAUDIO_ERROR_DISCONNECTED, -899);
        assert_eq!(AAUDIO_STREAM_STATE_OPEN, 2);
        assert_eq!(AAUDIO_STREAM_STATE_STARTED, 4);
        assert_eq!(AAUDIO_STREAM_STATE_PAUSED, 6);
        assert_eq!(AAUDIO_STREAM_STATE_FLUSHED, 8);
        assert_eq!(AAUDIO_STREAM_STATE_STOPPED, 10);
        assert_eq!(AAUDIO_STREAM_STATE_DISCONNECTED, 13);
    }

    #[test]
    fn f32_to_i16_clamps_nans_and_out_of_range() {
        let mut out = Vec::new();
        convert_f32_to_i16(&[-1.5, -1.0, 0.0, f32::NAN, 1.0, 2.5], &mut out);
        assert_eq!(out, vec![-32767, -32767, 0, 0, 32767, 32767]);
    }

    #[test]
    fn consumed_source_frames_maps_passthrough_and_resampled_tails() {
        assert_eq!(consumed_source_frames(500, 44_100, 44_100, 1024), 500);
        assert_eq!(consumed_source_frames(2000, 44_100, 44_100, 1024), 1024);
        // 480 dst frames at 48kHz corresponds to 441 src frames at 44.1kHz.
        assert_eq!(consumed_source_frames(480, 44_100, 48_000, 1024), 441);
    }

    #[test]
    #[ignore = "requires Android 8.0+ / Termux device with libaaudio.so"]
    fn live_aaudio_open_write_flush_drain_close() {
        let mut sink = AAudioSink::new().expect("libaaudio.so should load on Android");
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
