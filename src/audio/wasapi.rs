//! Windows WASAPI shared-mode audio output sink built on `windows` 0.62 COM bindings.

use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioRenderClient,
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
    eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    SPEAKER_BACK_LEFT, SPEAKER_BACK_RIGHT, SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT,
    SPEAKER_LOW_FREQUENCY, SPEAKER_SIDE_LEFT, SPEAKER_SIDE_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::core::{GUID, HRESULT, Interface, PCWSTR};

use crate::audio::AudioSink;
use crate::error::{Result, TunrError};

/// 100 ms in 100-nanosecond REFTIME units for shared-mode WASAPI buffering.
const BUFFER_DURATION_HNS: i64 = 1_000_000;
const EVENT_WAIT_MS: u32 = 500;
const MAX_WRITE_STALL: Duration = Duration::from_secs(3);
const MAX_DRAIN_WAIT: Duration = Duration::from_millis(1500);
const MAX_RECONNECT_ATTEMPTS: usize = 2;

struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init_mta() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            // S_OK or S_FALSE: thread COM apartment refcount incremented, must balance on drop.
            Ok(Self { should_uninit: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // Thread already initialized COM (e.g., STA); COM remains valid, do not uninitialize.
            Ok(Self { should_uninit: false })
        } else {
            Err(TunrError::Audio(format!(
                "CoInitializeEx(COINIT_MULTITHREADED) failed: {}",
                windows::core::Error::from_hresult(hr)
            )))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { CoUninitialize() };
        }
    }
}

struct EventHandle(HANDLE);

impl EventHandle {
    fn create_auto_reset() -> Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|err| TunrError::Audio(format!("CreateEventW failed: {err}")))?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Active WASAPI COM stream resources.
///
/// Field declaration order is significant: COM interfaces (`render_client`,
/// `audio_client`) and `event_handle` MUST drop before `_com_guard` calls `CoUninitialize()`.
struct WasapiStream {
    render_client: IAudioRenderClient,
    audio_client: IAudioClient,
    event_handle: EventHandle,
    buffer_frames: u32,
    _com_guard: ComGuard,
}

unsafe impl Send for WasapiStream {}

/// Blocking Windows WASAPI shared-mode PCM output sink implementing [`AudioSink`].
pub struct WasapiSink {
    stream: Option<WasapiStream>,
    spec: Option<(u32, u8)>,
    started: bool,
}

unsafe impl Send for WasapiSink {}

impl WasapiSink {
    /// Verifies that COM and a default Windows render endpoint are accessible without retaining
    /// COM state across thread handoff (`make_sink()` -> `tunr-engine`).
    pub fn new() -> Result<Self> {
        {
            let _com_guard = ComGuard::init_mta()?;
            let _endpoint = default_render_endpoint()?;
        }
        Ok(Self {
            stream: None,
            spec: None,
            started: false,
        })
    }

    fn open_stream(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        let com_guard = ComGuard::init_mta()?;
        let device = default_render_endpoint()?;
        let audio_client = unsafe { activate_audio_client(&device) }
            .map_err(|err| TunrError::Audio(format!("IMMDevice::Activate(IAudioClient) failed: {err}")))?;

        let wfext = build_float_waveformat(sample_rate, channels)?;
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

        // Shared-mode WASAPI requires hnsPeriodicity = 0.
        unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_DURATION_HNS,
                0,
                (&wfext as *const WAVEFORMATEXTENSIBLE).cast::<WAVEFORMATEX>(),
                None,
            )
        }
        .map_err(|err| TunrError::Audio(format!("IAudioClient::Initialize failed: {err}")))?;

        let buffer_frames = unsafe { audio_client.GetBufferSize() }
            .map_err(|err| TunrError::Audio(format!("IAudioClient::GetBufferSize failed: {err}")))?;
        if buffer_frames == 0 {
            return Err(TunrError::Audio(
                "WASAPI reported zero buffer frames".to_string(),
            ));
        }

        let event_handle = EventHandle::create_auto_reset()?;
        unsafe { audio_client.SetEventHandle(event_handle.raw()) }
            .map_err(|err| TunrError::Audio(format!("IAudioClient::SetEventHandle failed: {err}")))?;

        let render_client: IAudioRenderClient = unsafe { audio_client.GetService() }
            .map_err(|err| TunrError::Audio(format!("IAudioClient::GetService(IAudioRenderClient) failed: {err}")))?;

        self.stream = Some(WasapiStream {
            render_client,
            audio_client,
            event_handle,
            buffer_frames,
            _com_guard: com_guard,
        });
        self.spec = Some((sample_rate, channels));
        self.started = false;
        Ok(())
    }

    fn write_chunk_to_stream(
        &mut self,
        samples: &[f32],
        channels: usize,
        written_frames: &mut usize,
    ) -> std::result::Result<(), WasapiStepError> {
        let total_frames = samples.len() / channels;
        let mut stall_start: Option<Instant> = None;

        while *written_frames < total_frames {
            let stream = self
                .stream
                .as_ref()
                .ok_or_else(|| WasapiStepError::Fatal(TunrError::Audio("WASAPI stream not open".to_string())))?;

            let padding = unsafe { stream.audio_client.GetCurrentPadding() }.map_err(map_win_err)?;
            let available = stream.buffer_frames.saturating_sub(padding);

            if available > 0 {
                let remaining = total_frames - *written_frames;
                let to_write = remaining.min(available as usize) as u32;
                let raw_buf = unsafe { stream.render_client.GetBuffer(to_write) }.map_err(map_win_err)?;
                if raw_buf.is_null() {
                    return Err(WasapiStepError::Fatal(TunrError::Audio(
                        "IAudioRenderClient::GetBuffer returned null pointer".to_string(),
                    )));
                }
                let src_start = *written_frames * channels;
                let sample_count = (to_write as usize) * channels;
                unsafe {
                    ptr::copy_nonoverlapping(samples[src_start..].as_ptr(), raw_buf.cast::<f32>(), sample_count);
                }
                unsafe { stream.render_client.ReleaseBuffer(to_write, 0) }.map_err(map_win_err)?;
                *written_frames += to_write as usize;
                stall_start = None;

                if !self.started {
                    let _ = unsafe { ResetEvent(stream.event_handle.raw()) };
                    unsafe { stream.audio_client.Start() }.map_err(map_win_err)?;
                    self.started = true;
                }
                continue;
            }

            if !self.started {
                let _ = unsafe { ResetEvent(stream.event_handle.raw()) };
                unsafe { stream.audio_client.Start() }.map_err(map_win_err)?;
                self.started = true;
            }

            let wait = unsafe { WaitForSingleObject(stream.event_handle.raw(), EVENT_WAIT_MS) };
            if wait == WAIT_OBJECT_0 {
                stall_start = None;
                continue;
            }
            if wait == WAIT_TIMEOUT {
                let start = stall_start.get_or_insert_with(Instant::now);
                if start.elapsed() >= MAX_WRITE_STALL {
                    return Err(WasapiStepError::Fatal(TunrError::Audio(
                        "WASAPI render wait timed out without buffer progress".to_string(),
                    )));
                }
                continue;
            }
            return Err(WasapiStepError::Fatal(TunrError::Audio(format!(
                "WaitForSingleObject failed with status 0x{:08X}",
                wait.0
            ))));
        }

        Ok(())
    }
}

enum WasapiStepError {
    DeviceInvalidated,
    Fatal(TunrError),
}

fn map_win_err(err: windows::core::Error) -> WasapiStepError {
    if err.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        WasapiStepError::DeviceInvalidated
    } else {
        WasapiStepError::Fatal(TunrError::Audio(format!("WASAPI error: {err}")))
    }
}

fn default_render_endpoint() -> Result<IMMDevice> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|err| {
            TunrError::Audio(format!(
                "CoCreateInstance(MMDeviceEnumerator) failed: {err}"
            ))
        })?;
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(|err| {
        TunrError::Audio(format!(
            "GetDefaultAudioEndpoint(eRender, eConsole) failed: {err}"
        ))
    })
}

/// Calls `IMMDevice::Activate` via slot 3 of the COM vtable so `Win32_System_Com_StructuredStorage`
/// / `Win32_System_Variant` (`PROPVARIANT`) are not required for null `pActivationParams`.
unsafe fn activate_audio_client(device: &IMMDevice) -> std::result::Result<IAudioClient, windows::core::Error> {
    let raw_dev = <IMMDevice as Interface>::as_raw(device);
    let vtable = unsafe { *(raw_dev as *const *const *const c_void) };
    let activate_ptr = unsafe { *vtable.add(3) };
    let activate: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        windows::Win32::System::Com::CLSCTX,
        *const c_void,
        *mut *mut c_void,
    ) -> HRESULT = unsafe { std::mem::transmute(activate_ptr) };

    let mut out = ptr::null_mut();
    let hr = unsafe { activate(raw_dev, &IAudioClient::IID, CLSCTX_ALL, ptr::null(), &mut out) };
    hr.ok()?;
    if out.is_null() {
        return Err(windows::core::Error::from_hresult(HRESULT(0x8000_4003_u32 as i32)));
    }
    Ok(unsafe { IAudioClient::from_raw(out) })
}

fn channel_mask(channels: u8) -> u32 {
    match channels {
        1 => SPEAKER_FRONT_CENTER,
        2 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        4 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT | SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT,
        6 => {
            SPEAKER_FRONT_LEFT
                | SPEAKER_FRONT_RIGHT
                | SPEAKER_FRONT_CENTER
                | SPEAKER_LOW_FREQUENCY
                | SPEAKER_BACK_LEFT
                | SPEAKER_BACK_RIGHT
        }
        8 => {
            SPEAKER_FRONT_LEFT
                | SPEAKER_FRONT_RIGHT
                | SPEAKER_FRONT_CENTER
                | SPEAKER_LOW_FREQUENCY
                | SPEAKER_BACK_LEFT
                | SPEAKER_BACK_RIGHT
                | SPEAKER_SIDE_LEFT
                | SPEAKER_SIDE_RIGHT
        }
        _ => 0,
    }
}

fn build_float_waveformat(sample_rate: u32, channels: u8) -> Result<WAVEFORMATEXTENSIBLE> {
    if sample_rate == 0 {
        return Err(TunrError::Audio("WASAPI sample_rate must be > 0".to_string()));
    }
    if channels == 0 {
        return Err(TunrError::Audio("WASAPI channels must be > 0".to_string()));
    }
    let block_align = u16::from(channels) * 4;
    let avg_bytes_per_sec = sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| TunrError::Audio("WASAPI sample rate byte rate overflow".to_string()))?;
    let cb_size = (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16;

    Ok(WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: u16::from(channels),
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: avg_bytes_per_sec,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: cb_size,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: channel_mask(channels),
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    })
}

impl AudioSink for WasapiSink {
    fn open(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        if sample_rate == 0 {
            return Err(TunrError::Audio("WASAPI sample_rate must be > 0".to_string()));
        }
        if channels == 0 {
            return Err(TunrError::Audio("WASAPI channels must be > 0".to_string()));
        }

        if self.stream.is_some() && self.spec == Some((sample_rate, channels)) {
            return Ok(());
        }

        self.close();
        self.open_stream(sample_rate, channels)
    }

    fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
        let (sample_rate, channels_u8) = self
            .spec
            .ok_or_else(|| TunrError::Audio("WasapiSink::write_f32 called before open()".to_string()))?;
        let channels = usize::from(channels_u8);
        if samples.len() % channels != 0 {
            return Err(TunrError::Audio(format!(
                "PCM sample slice length {} is not divisible by {channels} channels",
                samples.len()
            )));
        }
        if samples.is_empty() {
            return Ok(());
        }

        let mut written_frames = 0usize;
        let mut reconnects = 0usize;

        while written_frames < samples.len() / channels {
            match self.write_chunk_to_stream(samples, channels, &mut written_frames) {
                Ok(()) => break,
                Err(WasapiStepError::Fatal(err)) => return Err(err),
                Err(WasapiStepError::DeviceInvalidated) => {
                    reconnects += 1;
                    if reconnects > MAX_RECONNECT_ATTEMPTS {
                        return Err(TunrError::Audio(
                            "WASAPI endpoint remained invalidated after reopen attempts".to_string(),
                        ));
                    }
                    self.stream = None;
                    self.started = false;
                    self.open_stream(sample_rate, channels_u8)?;
                }
            }
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let Some(stream) = self.stream.as_ref() else {
            return Ok(());
        };
        let stop_res = unsafe { stream.audio_client.Stop() };
        let reset_res = unsafe { stream.audio_client.Reset() };
        self.started = false;

        for res in [stop_res, reset_res] {
            if let Err(err) = res {
                if err.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                    self.close();
                    return Ok(());
                }
                return Err(TunrError::Audio(format!("WASAPI flush failed: {err}")));
            }
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        let Some(stream) = self.stream.as_ref() else {
            return Ok(());
        };
        if !self.started {
            return Ok(());
        }

        let (sample_rate, _) = self.spec.unwrap_or((48_000, 2));
        let est_ms = ((u64::from(stream.buffer_frames) * 1000) / u64::from(sample_rate.max(1))) + 150;
        let deadline = Instant::now() + Duration::from_millis(est_ms.clamp(100, MAX_DRAIN_WAIT.as_millis() as u64));

        while Instant::now() < deadline {
            match unsafe { stream.audio_client.GetCurrentPadding() } {
                Ok(0) => break,
                Ok(_) => {
                    let _ = unsafe { WaitForSingleObject(stream.event_handle.raw(), 25) };
                }
                Err(err) if err.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                    self.close();
                    return Ok(());
                }
                Err(err) => return Err(TunrError::Audio(format!("WASAPI drain failed: {err}"))),
            }
        }

        let _ = unsafe { stream.audio_client.Stop() };
        let _ = unsafe { stream.audio_client.Reset() };
        self.started = false;
        Ok(())
    }

    fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = unsafe { stream.audio_client.Stop() };
            drop(stream);
        }
        self.spec = None;
        self.started = false;
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveformat_extensible_abi_and_channel_masks() {
        assert_eq!(std::mem::size_of::<WAVEFORMATEX>(), 18);
        assert_eq!(std::mem::size_of::<WAVEFORMATEXTENSIBLE>(), 40);

        let stereo = build_float_waveformat(48_000, 2).unwrap();
        let format = stereo.Format;
        let tag = format.wFormatTag;
        let channels = format.nChannels;
        let sr = format.nSamplesPerSec;
        let align = format.nBlockAlign;
        let avg = format.nAvgBytesPerSec;
        let bits = format.wBitsPerSample;
        let cb = format.cbSize;
        let mask = stereo.dwChannelMask;
        let sub = stereo.SubFormat;

        assert_eq!(tag, WAVE_FORMAT_EXTENSIBLE as u16);
        assert_eq!(channels, 2);
        assert_eq!(sr, 48_000);
        assert_eq!(align, 8);
        assert_eq!(avg, 384_000);
        assert_eq!(bits, 32);
        assert_eq!(cb, 22);
        assert_eq!(mask, SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT);
        assert_eq!(sub, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);

        assert_eq!(channel_mask(1), SPEAKER_FRONT_CENTER);
        assert!(build_float_waveformat(0, 2).is_err());
        assert!(build_float_waveformat(44_100, 0).is_err());
    }

    #[test]
    #[ignore = "requires live Windows audio endpoint (WASAPI)"]
    fn live_wasapi_open_write_flush_drain_close() {
        let mut sink = WasapiSink::new().unwrap();
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
