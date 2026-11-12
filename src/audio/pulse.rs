use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomPinned;
use std::path::{Path, PathBuf};
use std::ptr::{NonNull, null};

use libloading::Library;

use crate::audio::AudioSink;
use crate::error::{Result, TermusicError};

const PA_SAMPLE_FLOAT32LE: c_int = 5;
const PA_STREAM_PLAYBACK: c_int = 1;

#[repr(C)]
struct PaSimple {
    _data: [u8; 0],
    _marker: PhantomPinned,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct PaSampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type PaSimpleNewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const PaSampleSpec,
    map: *const c_void,
    attr: *const PaBufferAttr,
    error: *mut c_int,
) -> *mut PaSimple;

type PaSimpleWriteFn = unsafe extern "C" fn(
    s: *mut PaSimple,
    data: *const c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;

type PaSimpleFlushFn = unsafe extern "C" fn(s: *mut PaSimple, error: *mut c_int) -> c_int;
type PaSimpleDrainFn = unsafe extern "C" fn(s: *mut PaSimple, error: *mut c_int) -> c_int;
type PaSimpleFreeFn = unsafe extern "C" fn(s: *mut PaSimple);
type PaStrerrorFn = unsafe extern "C" fn(error: c_int) -> *const c_char;

struct PulseApi {
    // Keep loaded library handles alive for as long as function pointers are held.
    _simple_lib: Library,
    _pulse_lib: Option<Library>,
    pa_simple_new: PaSimpleNewFn,
    pa_simple_write: PaSimpleWriteFn,
    pa_simple_flush: PaSimpleFlushFn,
    pa_simple_drain: PaSimpleDrainFn,
    pa_simple_free: PaSimpleFreeFn,
    pa_strerror: Option<PaStrerrorFn>,
}

const TERMUX_FALLBACK_LIB_DIR: &str = "/data/data/com.termux/files/usr/lib";

fn pulse_library_candidates(names: &[&str], prefix: Option<&Path>, include_termux_fallback: bool) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(p) = prefix.filter(|p| !p.as_os_str().is_empty()) {
        dirs.push(p.join("lib"));
    }
    if include_termux_fallback {
        let fallback = Path::new(TERMUX_FALLBACK_LIB_DIR);
        if !dirs.iter().any(|d| d == fallback) {
            dirs.push(fallback.to_path_buf());
        }
    }

    let mut candidates = Vec::with_capacity(names.len() * (1 + dirs.len()));
    for name in names {
        candidates.push(PathBuf::from(name));
    }
    for dir in dirs {
        for name in names {
            candidates.push(dir.join(name));
        }
    }
    candidates
}

fn load_pulse_library(names: &[&str]) -> std::result::Result<Library, libloading::Error> {
    let prefix = std::env::var_os("PREFIX").map(PathBuf::from);
    let candidates = pulse_library_candidates(names, prefix.as_deref(), cfg!(target_os = "android"));
    let mut last_err = None;
    for path in candidates {
        match unsafe { Library::new(&path) } {
            Ok(lib) => return Ok(lib),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.expect("names slice is non-empty"))
}

impl PulseApi {
    fn load() -> Result<Self> {
        let simple_lib = load_pulse_library(&["libpulse-simple.so.0", "libpulse-simple.so"]).map_err(|err| {
            let hint = if cfg!(target_os = "android") {
                " (on Termux install 'pulseaudio' and start 'pulseaudio --start --exit-idle-time=-1', or use AAudio)"
            } else {
                ""
            };
            TermusicError::Audio(format!(
                "failed to load libpulse-simple.so.0 (or libpulse-simple.so): {err}{hint}"
            ))
        })?;

        let pa_simple_new = load_symbol::<PaSimpleNewFn>(&simple_lib, b"pa_simple_new\0")?;
        let pa_simple_write = load_symbol::<PaSimpleWriteFn>(&simple_lib, b"pa_simple_write\0")?;
        let pa_simple_flush = load_symbol::<PaSimpleFlushFn>(&simple_lib, b"pa_simple_flush\0")?;
        let pa_simple_drain = load_symbol::<PaSimpleDrainFn>(&simple_lib, b"pa_simple_drain\0")?;
        let pa_simple_free = load_symbol::<PaSimpleFreeFn>(&simple_lib, b"pa_simple_free\0")?;

        let mut pulse_lib = None;
        let pa_strerror = match unsafe { simple_lib.get::<PaStrerrorFn>(b"pa_strerror\0") } {
            Ok(sym) => Some(*sym),
            Err(_) => {
                let loaded = load_pulse_library(&["libpulse.so.0", "libpulse.so"]).ok();
                let sym = loaded.as_ref().and_then(|lib| {
                    unsafe { lib.get::<PaStrerrorFn>(b"pa_strerror\0") }
                        .ok()
                        .map(|s| *s)
                });
                pulse_lib = loaded;
                sym
            }
        };

        Ok(Self {
            _simple_lib: simple_lib,
            _pulse_lib: pulse_lib,
            pa_simple_new,
            pa_simple_write,
            pa_simple_flush,
            pa_simple_drain,
            pa_simple_free,
            pa_strerror,
        })
    }
}

fn load_symbol<T: Copy>(lib: &Library, name: &[u8]) -> Result<T> {
    let sym = unsafe { lib.get::<T>(name) }.map_err(|err| {
        let label = String::from_utf8_lossy(name).trim_end_matches('\0').to_string();
        TermusicError::Audio(format!("failed to resolve PulseAudio symbol '{label}': {err}"))
    })?;
    Ok(*sym)
}

fn buffer_attr_for_spec(sample_rate: u32, channels: u8) -> PaBufferAttr {
    let frame_bytes = u64::from(channels).max(1) * std::mem::size_of::<f32>() as u64;
    let bytes_per_sec = u64::from(sample_rate).saturating_mul(frame_bytes);
    // ~120ms target buffer keeps flush/pause/seek responsive while avoiding underruns.
    let target_bytes = (bytes_per_sec.saturating_mul(120)) / 1000;
    let min_bytes = frame_bytes.max(4096);
    let aligned = (target_bytes.max(min_bytes) / frame_bytes) * frame_bytes;
    let tlength = u32::try_from(aligned.min(u64::from(u32::MAX - 1))).unwrap_or(u32::MAX - 1);

    PaBufferAttr {
        maxlength: u32::MAX,
        tlength,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: u32::MAX,
    }
}

/// PulseAudio playback sink backed by runtime `dlopen` of `libpulse-simple.so.0`.
///
/// # Concurrency & Ownership Contract
/// - `PulseSimpleSink` is [`Send`] (so ownership can move into the dedicated
///   `termusic-engine` OS thread) and intentionally `!Sync` (due to `NonNull<PaSimple>`).
/// - `libpulse-simple` owns an internal threaded mainloop whose `pa_simple*` operations are
///   NOT safe under concurrent shared access; all playback calls require exclusive `&mut self`
///   access from a single owner thread.
/// - Tracks the currently open `(sample_rate, channels)` specification and only reopens the
///   underlying stream when parameters change or after `close()`.
///
/// # Shutdown & Drop Order Invariant
/// `stream` is declared before `api`, and [`Drop::drop`] invokes [`AudioSink::close`] first so
/// `pa_simple_free` always closes the active stream before `PulseApi` drops and unloads
/// `libpulse-simple` / `libpulse`. Calling `close()` repeatedly (for example on engine stop
/// followed by `Drop` during async daemon shutdown) is idempotent.
pub struct PulseSimpleSink {
    // Declared before `api` so any stream handle is freed before unloading libraries.
    stream: Option<NonNull<PaSimple>>,
    spec: Option<(u32, u8)>,
    app_name: CString,
    stream_name: CString,
    api: PulseApi,
}

// SAFETY: `PulseSimpleSink` is moved once to the `termusic-engine` OS thread and never shares
// `&self` (`!Sync` is preserved via `NonNull<PaSimple>`). `libpulse-simple` manages its internal
// threaded mainloop per `pa_simple*`, and `&mut self` guarantees serialized single-owner calls
// with `PulseApi` libraries kept alive strictly longer than `stream`.
unsafe impl Send for PulseSimpleSink {}

impl PulseSimpleSink {
    /// Load `libpulse-simple.so.0` (falling back to `libpulse-simple.so`) and prepare
    /// a PulseAudio sink without connecting a playback stream until [`AudioSink::open`] is called.
    pub fn new() -> Result<Self> {
        Self::with_names("termusic", "playback")
    }

    /// Load `libpulse-simple` with custom PulseAudio application and stream names.
    pub fn with_names(app_name: &str, stream_name: &str) -> Result<Self> {
        let app_c = CString::new(app_name).map_err(|err| {
            TermusicError::InvalidArg(format!("invalid PulseAudio app name: {err}"))
        })?;
        let stream_c = CString::new(stream_name).map_err(|err| {
            TermusicError::InvalidArg(format!("invalid PulseAudio stream name: {err}"))
        })?;
        let api = PulseApi::load()?;

        Ok(Self {
            stream: None,
            spec: None,
            app_name: app_c,
            stream_name: stream_c,
            api,
        })
    }

    /// Return the currently open `(sample_rate, channels)` specification, if connected.
    pub fn current_spec(&self) -> Option<(u32, u8)> {
        self.spec
    }

    /// Return whether a PulseAudio stream is currently open.
    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    fn format_pa_error(&self, code: c_int) -> String {
        if let Some(pa_strerror) = self.api.pa_strerror {
            let ptr = unsafe { pa_strerror(code) };
            if !ptr.is_null() {
                let msg = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
                return format!("{msg} (code {code})");
            }
        }
        format!("PulseAudio error code {code}")
    }
}

impl AudioSink for PulseSimpleSink {
    fn open(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        if sample_rate == 0 || channels == 0 {
            return Err(TermusicError::Audio(format!(
                "invalid audio specification: {sample_rate} Hz, {channels} channels"
            )));
        }

        if self.stream.is_some() && self.spec == Some((sample_rate, channels)) {
            return Ok(());
        }

        self.close();

        let sample_spec = PaSampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: sample_rate,
            channels,
        };
        let buf_attr = buffer_attr_for_spec(sample_rate, channels);
        let mut err: c_int = 0;

        let ptr = unsafe {
            (self.api.pa_simple_new)(
                null(),
                self.app_name.as_ptr(),
                PA_STREAM_PLAYBACK,
                null(),
                self.stream_name.as_ptr(),
                &sample_spec,
                null(),
                &buf_attr,
                &mut err,
            )
        };

        let Some(stream) = NonNull::new(ptr) else {
            return Err(TermusicError::Audio(format!(
                "pa_simple_new failed ({sample_rate} Hz, {channels} ch): {}",
                self.format_pa_error(err)
            )));
        };

        self.stream = Some(stream);
        self.spec = Some((sample_rate, channels));
        Ok(())
    }

    fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
        let stream = self.stream.ok_or_else(|| {
            TermusicError::Audio("PulseAudio stream is not open".to_string())
        })?;
        if samples.is_empty() {
            return Ok(());
        }

        let bytes = std::mem::size_of_val(samples);
        let mut err: c_int = 0;
        let rc = unsafe {
            (self.api.pa_simple_write)(
                stream.as_ptr(),
                samples.as_ptr().cast::<c_void>(),
                bytes,
                &mut err,
            )
        };
        if rc < 0 {
            return Err(TermusicError::Audio(format!(
                "pa_simple_write failed: {}",
                self.format_pa_error(err)
            )));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let Some(stream) = self.stream else {
            return Ok(());
        };
        let mut err: c_int = 0;
        let rc = unsafe { (self.api.pa_simple_flush)(stream.as_ptr(), &mut err) };
        if rc < 0 {
            return Err(TermusicError::Audio(format!(
                "pa_simple_flush failed: {}",
                self.format_pa_error(err)
            )));
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        let Some(stream) = self.stream else {
            return Ok(());
        };
        let mut err: c_int = 0;
        let rc = unsafe { (self.api.pa_simple_drain)(stream.as_ptr(), &mut err) };
        if rc < 0 {
            return Err(TermusicError::Audio(format!(
                "pa_simple_drain failed: {}",
                self.format_pa_error(err)
            )));
        }
        Ok(())
    }

    fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            // SAFETY: `.take()` clears `self.stream` before `pa_simple_free`, guaranteeing the
            // non-null stream is freed at most once across repeated `close()` calls and `Drop`,
            // while `self.api` remains loaded until after `Drop::drop` completes.
            unsafe {
                (self.api.pa_simple_free)(stream.as_ptr());
            }
        }
        self.spec = None;
    }
}

impl Drop for PulseSimpleSink {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn c_abi_layouts_match_pulseaudio_headers() {
        assert_eq!(size_of::<PaSampleSpec>(), 12);
        assert_eq!(align_of::<PaSampleSpec>(), 4);
        assert_eq!(size_of::<PaBufferAttr>(), 20);
        assert_eq!(align_of::<PaBufferAttr>(), 4);
    }

    #[test]
    fn buffer_attr_computes_120ms_frame_aligned_tlength() {
        let attr = buffer_attr_for_spec(44_100, 2);
        assert_eq!(attr.tlength, 42_336);
        assert_eq!(attr.maxlength, u32::MAX);
        assert_eq!(attr.prebuf, u32::MAX);
        assert_eq!(attr.minreq, u32::MAX);
        assert_eq!(attr.fragsize, u32::MAX);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn loads_pulse_simple_symbols_without_opening_stream() {
        let sink = PulseSimpleSink::new().expect("libpulse-simple.so.0 should load on host");
        assert!(!sink.is_open());
        assert_eq!(sink.current_spec(), None);
        assert!(sink.api.pa_strerror.is_some());
    }

    #[test]
    fn pulse_library_candidates_include_prefix_and_termux_fallback() {
        let candidates = pulse_library_candidates(
            &["libpulse-simple.so.0", "libpulse-simple.so"],
            Some(Path::new("/data/data/com.termux/files/usr")),
            true,
        );
        assert_eq!(candidates[0], PathBuf::from("libpulse-simple.so.0"));
        assert_eq!(candidates[1], PathBuf::from("libpulse-simple.so"));
        assert_eq!(
            candidates[2],
            PathBuf::from("/data/data/com.termux/files/usr/lib/libpulse-simple.so.0")
        );
        assert_eq!(
            candidates[3],
            PathBuf::from("/data/data/com.termux/files/usr/lib/libpulse-simple.so")
        );
        // Should not duplicate TERMUX_FALLBACK_LIB_DIR when PREFIX already points there.
        assert_eq!(candidates.len(), 4);

        let with_custom_prefix = pulse_library_candidates(
            &["libpulse.so.0"],
            Some(Path::new("/custom/termux/usr")),
            true,
        );
        assert_eq!(
            with_custom_prefix,
            vec![
                PathBuf::from("libpulse.so.0"),
                PathBuf::from("/custom/termux/usr/lib/libpulse.so.0"),
                PathBuf::from("/data/data/com.termux/files/usr/lib/libpulse.so.0"),
            ]
        );
    }

    #[test]
    fn pulse_sink_is_send_and_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<PulseSimpleSink>();

        #[allow(dead_code)]
        trait NotSyncGuard1 {
            fn assert_not_sync() {}
        }
        impl<T: Sync + ?Sized> NotSyncGuard1 for T {}

        trait NotSyncGuard2 {
            fn assert_not_sync() {}
        }
        impl<T: ?Sized> NotSyncGuard2 for T {}

        // Compiles only when `PulseSimpleSink: !Sync` (otherwise `E0034` ambiguity).
        PulseSimpleSink::assert_not_sync();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn close_and_idle_controls_are_idempotent() {
        let mut sink = PulseSimpleSink::new().expect("libpulse-simple.so.0 should load on host");
        sink.flush().unwrap();
        sink.drain().unwrap();
        sink.close();
        sink.close();
        assert!(!sink.is_open());
        assert_eq!(sink.current_spec(), None);
    }

    #[test]
    #[ignore = "requires live PulseAudio / WSLg audio server"]
    fn live_pulse_open_write_flush_drain_close() {
        let mut sink = PulseSimpleSink::new().unwrap();
        sink.open(44_100, 2).unwrap();
        assert!(sink.is_open());
        assert_eq!(sink.current_spec(), Some((44_100, 2)));
        sink.write_f32(&vec![0.0; 1024]).unwrap();
        sink.flush().unwrap();
        sink.open(44_100, 2).unwrap();
        sink.drain().unwrap();
        sink.close();
        assert!(!sink.is_open());
    }
}
