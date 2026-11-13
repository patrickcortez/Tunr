use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, MediaInfo, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag, Tag};
use symphonia::core::units::{Duration, Time, TimeBase};

use crate::error::{Result, TunrError};

#[cfg(target_os = "android")]
pub mod aaudio;
#[cfg(target_os = "macos")]
pub mod audioqueue;
pub mod null_sink;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod pulse;
pub mod resample;
#[cfg(target_os = "windows")]
pub mod wasapi;

/// Return `true` if `path` has a supported audio file extension (`mp3`, `flac`, `wav`, `ogg`,
/// `m4a`, `aac`, case-insensitive).
pub fn is_supported_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    ext.eq_ignore_ascii_case("mp3")
        || ext.eq_ignore_ascii_case("flac")
        || ext.eq_ignore_ascii_case("wav")
        || ext.eq_ignore_ascii_case("ogg")
        || ext.eq_ignore_ascii_case("m4a")
        || ext.eq_ignore_ascii_case("aac")
}

/// Extracted metadata and duration for an audio track probed via Symphonia.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackProbe {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
}

/// Sink abstraction for interleaved `f32` PCM audio output.
///
/// Sinks are [`Send`] so ownership can be transferred into the dedicated `tunr-engine` OS
/// thread, where every method is called with exclusive `&mut self` access. Implementations must
/// treat [`AudioSink::close`] as idempotent so repeated `close()` calls and [`Drop`] teardown
/// during daemon shutdown are safe.
pub trait AudioSink: Send {
    fn open(&mut self, sample_rate: u32, channels: u8) -> Result<()>;
    fn write_f32(&mut self, samples: &[f32]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    fn drain(&mut self) -> Result<()>;
    fn close(&mut self);
}

const MAX_VOLUME: u8 = 100;

/// Map a `[0, 100]` volume percentage to a perceptual cubic linear amplitude gain in `[0.0, 1.0]`.
pub fn volume_to_gain(volume: u8) -> f32 {
    if volume == 0 {
        0.0
    } else if volume >= MAX_VOLUME {
        1.0
    } else {
        (f32::from(volume) / f32::from(MAX_VOLUME)).powi(3)
    }
}

/// Scale interleaved `f32` PCM samples from `src` into `dst` with optional per-frame linear gain
/// ramping from `start_gain` toward `target_gain` over `ramp_frames` frames, returning the final
/// gain reached at the end of the chunk.
pub fn scale_pcm_f32_ramped(
    src: &[f32],
    dst: &mut Vec<f32>,
    channels: usize,
    start_gain: f32,
    target_gain: f32,
    ramp_frames: usize,
) -> f32 {
    dst.resize(src.len(), 0.0);
    if src.is_empty() || channels == 0 {
        for (d, &s) in dst.iter_mut().zip(src.iter()) {
            *d = s * target_gain;
        }
        return target_gain;
    }

    let total_frames = src.len() / channels;
    if total_frames == 0 || (start_gain - target_gain).abs() <= f32::EPSILON || ramp_frames == 0 {
        for (d, &s) in dst.iter_mut().zip(src.iter()) {
            *d = s * target_gain;
        }
        return target_gain;
    }

    let steps = total_frames.min(ramp_frames);
    let delta = target_gain - start_gain;
    let ramp_denom = ramp_frames as f32;

    for frame_idx in 0..steps {
        let t = (frame_idx + 1) as f32 / ramp_denom;
        let gain = start_gain + delta * t;
        let start = frame_idx * channels;
        let end = start + channels;
        for (d, &s) in dst[start..end].iter_mut().zip(&src[start..end]) {
            *d = s * gain;
        }
    }

    let rem_start = steps * channels;
    for (d, &s) in dst[rem_start..].iter_mut().zip(&src[rem_start..]) {
        *d = s * target_gain;
    }

    if total_frames < ramp_frames {
        start_gain + delta * (steps as f32 / ramp_denom)
    } else {
        target_gain
    }
}

/// Construct the platform default [`AudioSink`], honoring `TUNR_AUDIO_BACKEND`
/// (`auto`, `null`, `pulse`; platform-specific backends such as `aaudio`, `wasapi`, and
/// `audioqueue` are reported explicitly until implemented).
pub fn default_sink() -> Result<Box<dyn AudioSink>> {
    let backend = std::env::var("TUNR_AUDIO_BACKEND").ok();
    default_sink_for_backend(backend.as_deref())
}

fn default_sink_for_backend(backend: Option<&str>) -> Result<Box<dyn AudioSink>> {
    let selected = backend
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("auto")
        .to_ascii_lowercase();

    match selected.as_str() {
        "null" => Ok(Box::new(null_sink::NullSink::default())),
        "auto" => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(pulse::PulseSimpleSink::new()?))
            }
            #[cfg(target_os = "android")]
            {
                let prefer_pulse = std::env::var_os("PULSE_SERVER").is_some_and(|v| !v.is_empty());
                if prefer_pulse {
                    match pulse::PulseSimpleSink::new() {
                        Ok(sink) => Ok(Box::new(sink)),
                        Err(pulse_err) => match aaudio::AAudioSink::new() {
                            Ok(sink) => Ok(Box::new(sink)),
                            Err(aaudio_err) => Err(TunrError::Audio(format!(
                                "failed to initialize Termux PulseAudio ({pulse_err}) and AAudio ({aaudio_err})"
                            ))),
                        },
                    }
                } else {
                    match aaudio::AAudioSink::new() {
                        Ok(sink) => Ok(Box::new(sink)),
                        Err(aaudio_err) => match pulse::PulseSimpleSink::new() {
                            Ok(sink) => Ok(Box::new(sink)),
                            Err(pulse_err) => Err(TunrError::Audio(format!(
                                "failed to initialize AAudio ({aaudio_err}) and Termux PulseAudio ({pulse_err})"
                            ))),
                        },
                    }
                }
            }
            #[cfg(target_os = "windows")]
            {
                Ok(Box::new(wasapi::WasapiSink::new()?))
            }
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(audioqueue::AudioQueueSink::new()?))
            }
            #[cfg(not(any(
                target_os = "linux",
                target_os = "android",
                target_os = "windows",
                target_os = "macos"
            )))]
            {
                Err(TunrError::Audio(format!(
                    "no default audio backend is available for target OS '{}' (use TUNR_AUDIO_BACKEND=null for headless playback)",
                    std::env::consts::OS
                )))
            }
        }
        "pulse" => {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                Ok(Box::new(pulse::PulseSimpleSink::new()?))
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                Err(TunrError::Audio(format!(
                    "audio backend 'pulse' is only supported on Linux and Android (current target OS: '{}')",
                    std::env::consts::OS
                )))
            }
        }
        "aaudio" => {
            #[cfg(target_os = "android")]
            {
                Ok(Box::new(aaudio::AAudioSink::new()?))
            }
            #[cfg(not(target_os = "android"))]
            {
                Err(TunrError::Audio(format!(
                    "audio backend 'aaudio' is only supported on Android (current target OS: '{}')",
                    std::env::consts::OS
                )))
            }
        }
        "wasapi" => {
            #[cfg(target_os = "windows")]
            {
                Ok(Box::new(wasapi::WasapiSink::new()?))
            }
            #[cfg(not(target_os = "windows"))]
            {
                Err(TunrError::Audio(format!(
                    "audio backend 'wasapi' is only supported on Windows (current target OS: '{}')",
                    std::env::consts::OS
                )))
            }
        }
        "audioqueue" => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(audioqueue::AudioQueueSink::new()?))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(TunrError::Audio(format!(
                    "audio backend 'audioqueue' is only supported on macOS (current target OS: '{}')",
                    std::env::consts::OS
                )))
            }
        }
        other => Err(TunrError::Audio(format!(
            "unknown TUNR_AUDIO_BACKEND '{other}' (expected one of: auto, null, pulse, aaudio, wasapi, audioqueue)"
        ))),
    }
}

/// Owned chunk of decoded interleaved `f32` PCM samples with its presentation timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedChunk {
    pub samples: Vec<f32>,
    pub position_secs: f64,
}

/// Demuxer and decoder state for a single audio track opened via Symphonia.
///
/// Intended for single-owner `&mut self` decoding on the `tunr-engine` playback thread;
/// dropping releases the underlying demuxer, decoder, and container file handle cleanly.
pub struct SymphoniaTrack {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u8,
    time_base: Option<TimeBase>,
    duration_secs: Option<f64>,
    pcm_buf: Vec<f32>,
}

/// Probe `path` with Symphonia and extract basic tags (`title`, `artist`, `album`) and duration.
pub fn probe_track(path: &Path) -> Result<TrackProbe> {
    let mut format = probe_format(path)?;

    let (track_id, duration_secs) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| TunrError::Audio(format!("no audio track found in '{}'", path.display())))?;
        let id = track.id;
        let track_dur = compute_track_duration_secs(track);
        (id, track_dur)
    };
    let duration_secs = duration_secs.or_else(|| compute_media_duration_secs(format.media_info()));

    let mut collected = CollectedTags::default();

    let mut meta = format.metadata();
    loop {
        if let Some(rev) = meta.current() {
            collect_standard_tags(&rev.media.tags, &mut collected);
            for per_track in &rev.per_track {
                if per_track.track_id == u64::from(track_id) {
                    collect_standard_tags(&per_track.metadata.tags, &mut collected);
                }
            }
        }
        if meta.pop().is_none() {
            break;
        }
    }

    Ok(collected.into_track_probe(duration_secs))
}

impl SymphoniaTrack {
    /// Open `path`, probe its container format, and initialize an audio decoder for the default
    /// audio track.
    pub fn open(path: &Path) -> Result<Self> {
        let format = probe_format(path)?;
        let (track_id, time_base, audio_params, track_dur) = {
            let track = format
                .default_track(TrackType::Audio)
                .ok_or_else(|| TunrError::Audio(format!("no audio track found in '{}'", path.display())))?;
            let audio_params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or_else(|| {
                    TunrError::Audio(format!(
                        "missing audio codec parameters in '{}'",
                        path.display()
                    ))
                })?
                .clone();
            (
                track.id,
                track.time_base,
                audio_params,
                compute_track_duration_secs(track),
            )
        };

        let duration_secs = track_dur.or_else(|| compute_media_duration_secs(format.media_info()));

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
            .map_err(|err| {
                map_symphonia_err(&format!("failed to create audio decoder for '{}'", path.display()), err)
            })?;

        let (sample_rate, channels) = extract_audio_spec(&audio_params, decoder.codec_params())?;

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            time_base,
            duration_secs,
            pcm_buf: Vec::new(),
        })
    }

    /// Return the track sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Return the track channel count.
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Return the track duration in seconds, if known from the container.
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration_secs
    }

    /// Decode the next non-empty audio packet into interleaved `f32` PCM samples and return
    /// `Ok(Some((samples, position_secs)))`, or `Ok(None)` at EOF.
    ///
    /// Recoverable packet decode errors are skipped; `Error::ResetRequired` re-synchronizes the
    /// decoder cleanly.
    pub fn next_pcm_f32(&mut self) -> Result<Option<(&[f32], f64)>> {
        let pos_secs = loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                Err(SymError::ResetRequired) => {
                    self.reset_after_stream_change()?;
                    continue;
                }
                Err(SymError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(err) => return Err(map_symphonia_err("demux error", err)),
            };

            while !self.format.metadata().is_latest() {
                self.format.metadata().pop();
            }

            if packet.track_id != self.track_id {
                continue;
            }

            let position_secs = self
                .time_base
                .and_then(|tb| tb.calc_time(packet.pts))
                .map(|t| t.as_secs_f64().max(0.0))
                .unwrap_or(0.0);

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.frames() == 0 {
                        continue;
                    }
                    let needed = decoded.samples_interleaved();
                    if needed == 0 {
                        continue;
                    }

                    let spec = decoded.spec();
                    if spec.rate() > 0 {
                        self.sample_rate = spec.rate();
                    }
                    if let Ok(ch) = u8::try_from(spec.channels().count())
                        && ch > 0
                    {
                        self.channels = ch;
                    }

                    self.pcm_buf.resize(needed, 0.0);
                    decoded.copy_to_slice_interleaved(&mut self.pcm_buf);
                    break position_secs;
                }
                Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(err) => return Err(map_symphonia_err("decode error", err)),
            }
        };

        Ok(Some((&self.pcm_buf, pos_secs)))
    }

    /// Convenience wrapper over [`Self::next_pcm_f32`] returning an owned [`DecodedChunk`].
    pub fn next_chunk(&mut self) -> Result<Option<DecodedChunk>> {
        match self.next_pcm_f32()? {
            Some((samples, position_secs)) => Ok(Some(DecodedChunk {
                samples: samples.to_vec(),
                position_secs,
            })),
            None => Ok(None),
        }
    }

    /// Seek accurately to `target_secs` (clamped at `>= 0.0`), reset the decoder, and return the
    /// actual timestamp seeked to in seconds.
    pub fn seek(&mut self, target_secs: f64) -> Result<f64> {
        if !target_secs.is_finite() {
            return Err(TunrError::InvalidArg(
                "seek target must be a finite number of seconds".to_string(),
            ));
        }

        let clamped = target_secs.max(0.0);
        let time = Time::try_from_secs_f64(clamped).ok_or_else(|| {
            TunrError::Audio(format!("seek target out of representable range: {clamped}"))
        })?;

        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|err| map_symphonia_err("seek failed", err))?;

        self.decoder.reset();
        self.pcm_buf.clear();

        let actual_secs = self
            .time_base
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map(|t| t.as_secs_f64().max(0.0))
            .unwrap_or(clamped);

        Ok(actual_secs)
    }

    fn reset_after_stream_change(&mut self) -> Result<()> {
        let (track_id, time_base, audio_params, track_dur) = {
            let track = self
                .format
                .default_track(TrackType::Audio)
                .ok_or_else(|| TunrError::Audio("no audio track after stream reset".to_string()))?;
            let audio_params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or_else(|| {
                    TunrError::Audio("missing audio codec parameters after stream reset".to_string())
                })?
                .clone();
            (
                track.id,
                track.time_base,
                audio_params,
                compute_track_duration_secs(track),
            )
        };

        let duration_secs =
            track_dur.or_else(|| compute_media_duration_secs(self.format.media_info()));
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
            .map_err(|err| map_symphonia_err("failed to recreate decoder after stream reset", err))?;
        let (sample_rate, channels) = extract_audio_spec(&audio_params, decoder.codec_params())?;

        self.track_id = track_id;
        self.time_base = time_base;
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.duration_secs = duration_secs;
        self.decoder = decoder;
        self.pcm_buf.clear();
        Ok(())
    }
}

fn probe_format(path: &Path) -> Result<Box<dyn FormatReader>> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|err| map_symphonia_err(&format!("failed to probe '{}'", path.display()), err))
}

fn compute_track_duration_secs(track: &Track) -> Option<f64> {
    let tb = track.time_base?;
    let time = if let Some(dur) = track.duration {
        tb.calc_duration(dur)
    } else if let Some(num_frames) = track.num_frames {
        tb.calc_duration(Duration::from(num_frames))
    } else {
        None
    }?;
    let secs = time.as_secs_f64();
    (secs.is_finite() && secs >= 0.0).then_some(secs)
}

fn compute_media_duration_secs(media_info: &MediaInfo) -> Option<f64> {
    let tb = media_info.time_base?;
    let dur = media_info.duration?;
    let secs = tb.calc_duration(dur)?.as_secs_f64();
    (secs.is_finite() && secs >= 0.0).then_some(secs)
}

fn extract_audio_spec(
    track_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    decoder_params: &symphonia::core::codecs::audio::AudioCodecParameters,
) -> Result<(u32, u8)> {
    let sample_rate = track_params
        .sample_rate
        .or(decoder_params.sample_rate)
        .filter(|&r| r > 0)
        .ok_or_else(|| TunrError::Audio("missing or zero audio sample rate".to_string()))?;

    let ch_count = track_params
        .channels
        .as_ref()
        .or(decoder_params.channels.as_ref())
        .map(|c| c.count())
        .filter(|&c| c > 0)
        .ok_or_else(|| TunrError::Audio("missing or zero audio channel count".to_string()))?;

    let channels = u8::try_from(ch_count).map_err(|_| {
        TunrError::Audio(format!("unsupported channel count {ch_count} (exceeds u8::MAX)"))
    })?;

    Ok((sample_rate, channels))
}

#[derive(Default)]
struct CollectedTags {
    title: Option<String>,
    artist: Option<String>,
    album_artist: Option<String>,
    performer: Option<String>,
    album: Option<String>,
}

impl CollectedTags {
    #[cfg(test)]
    fn resolved_artist(&self) -> Option<String> {
        self.artist
            .as_ref()
            .or(self.album_artist.as_ref())
            .or(self.performer.as_ref())
            .cloned()
    }

    fn into_track_probe(self, duration_secs: Option<f64>) -> TrackProbe {
        TrackProbe {
            title: self.title,
            artist: self.artist.or(self.album_artist).or(self.performer),
            album: self.album,
            duration_secs,
        }
    }
}

fn collect_standard_tags(tags: &[Tag], out: &mut CollectedTags) {
    for tag in tags {
        let Some(std_tag) = &tag.std else {
            continue;
        };
        match std_tag {
            StandardTag::TrackTitle(val) => set_non_empty(&mut out.title, val),
            StandardTag::Artist(val) => set_non_empty(&mut out.artist, val),
            StandardTag::AlbumArtist(val) => set_non_empty(&mut out.album_artist, val),
            StandardTag::Performer(val) => set_non_empty(&mut out.performer, val),
            StandardTag::Album(val) => set_non_empty(&mut out.album, val),
            _ => {}
        }
    }
}

fn set_non_empty(slot: &mut Option<String>, val: &str) {
    let trimmed = val.trim();
    if !trimmed.is_empty() {
        *slot = Some(trimmed.to_string());
    }
}

fn map_symphonia_err(context: &str, err: SymError) -> TunrError {
    match err {
        SymError::IoError(io_err) => TunrError::Io(io_err),
        other => TunrError::Audio(format!("{context}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_wav_path(label: &str) -> PathBuf {
        let idx = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tunr-audio-test-{}-{idx}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{label}.wav"))
    }

    fn write_stereo_wav(path: &Path, sample_rate: u32, duration_secs: f32) {
        let channels: u16 = 2;
        let bits_per_sample: u16 = 16;
        let num_frames = (sample_rate as f32 * duration_secs).round() as u32;
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
            let left = ((2.0 * PI * 440.0 * t).sin() * 0.25 * i16::MAX as f32) as i16;
            let right = ((2.0 * PI * 660.0 * t).sin() * 0.25 * i16::MAX as f32) as i16;
            buf.extend_from_slice(&left.to_le_bytes());
            buf.extend_from_slice(&right.to_le_bytes());
        }

        fs::write(path, buf).unwrap();
    }

    #[test]
    fn supported_extensions_are_case_insensitive() {
        for ext in ["mp3", "MP3", "flac", "Flac", "wav", "WAV", "ogg", "m4a", "aac"] {
            assert!(is_supported_extension(Path::new(&format!("track.{ext}"))));
        }
        for ext in ["txt", "png", "json", "rs", "opus"] {
            assert!(!is_supported_extension(Path::new(&format!("track.{ext}"))));
        }
        assert!(!is_supported_extension(Path::new("no_extension")));
    }

    #[test]
    fn probe_and_decode_and_seek_wav() {
        let wav = temp_wav_path("probe_decode");
        write_stereo_wav(&wav, 44_100, 1.5);

        let probed = probe_track(&wav).unwrap();
        let dur = probed.duration_secs.expect("wav duration should be probed");
        assert!((dur - 1.5).abs() < 0.01, "unexpected duration {dur}");

        let mut track = SymphoniaTrack::open(&wav).unwrap();
        assert_eq!(track.sample_rate(), 44_100);
        assert_eq!(track.channels(), 2);
        assert!((track.duration_secs().unwrap() - 1.5).abs() < 0.01);

        let mut total_samples = 0usize;
        while let Some((samples, pos)) = track.next_pcm_f32().unwrap() {
            assert!(!samples.is_empty());
            assert_eq!(samples.len() % 2, 0);
            assert!(pos >= 0.0);
            total_samples += samples.len();
        }
        assert_eq!(total_samples, 44_100 * 3); // 1.5s * 2ch * 44100Hz

        // Seek back to 0.75s and verify decoded frames cover the remainder.
        let seeked = track.seek(0.75).unwrap();
        assert!(seeked <= 0.76, "accurate seek landed after target: {seeked}");
        let mut remainder_samples = 0usize;
        while let Some((samples, _pos)) = track.next_pcm_f32().unwrap() {
            remainder_samples += samples.len();
        }
        assert!(
            remainder_samples >= 44_100,
            "expected >= 0.5s of stereo samples after 0.75s seek, got {remainder_samples}"
        );

        let _ = fs::remove_dir_all(wav.parent().unwrap());
    }

    #[test]
    fn default_sink_for_backend_handles_null_unknown_and_target_constraints() {
        let mut sink = default_sink_for_backend(Some("  NULL  ")).unwrap();
        sink.open(44_100, 2).unwrap();
        sink.write_f32(&[0.0; 16]).unwrap();
        sink.flush().unwrap();
        sink.drain().unwrap();
        sink.close();

        let unknown_err = default_sink_for_backend(Some("bogus"))
            .err()
            .expect("unknown backend must error");
        assert!(matches!(unknown_err, TunrError::Audio(ref msg) if msg.contains("unknown TUNR_AUDIO_BACKEND")));

        #[cfg(not(target_os = "android"))]
        {
            let err = default_sink_for_backend(Some("aaudio")).err().expect("aaudio is Android-only");
            assert!(matches!(err, TunrError::Audio(ref msg) if msg.contains("aaudio")));
        }

        #[cfg(not(target_os = "windows"))]
        {
            let err = default_sink_for_backend(Some("wasapi")).err().expect("wasapi is Windows-only");
            assert!(matches!(err, TunrError::Audio(ref msg) if msg.contains("wasapi")));
        }

        #[cfg(not(target_os = "macos"))]
        {
            let err = default_sink_for_backend(Some("audioqueue"))
                .err()
                .expect("audioqueue is macOS-only");
            assert!(matches!(err, TunrError::Audio(ref msg) if msg.contains("audioqueue")));
        }

        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let err = default_sink_for_backend(Some("pulse"))
                .err()
                .expect("pulse is Linux/Android-only");
            assert!(matches!(err, TunrError::Audio(ref msg) if msg.contains("pulse")));
        }

        #[cfg(target_os = "linux")]
        {
            assert!(default_sink_for_backend(None).is_ok());
            assert!(default_sink_for_backend(Some("auto")).is_ok());
            assert!(default_sink_for_backend(Some("pulse")).is_ok());
        }

        #[cfg(target_os = "macos")]
        {
            assert!(default_sink_for_backend(Some("audioqueue")).is_ok());
        }
    }

    #[test]
    fn collect_standard_tags_prefers_artist_then_album_artist_then_performer() {
        use std::sync::Arc;
        use symphonia::core::meta::RawTag;

        let std_tag = |tag: StandardTag| Tag::new_std(RawTag::new("TAG", "val"), tag);

        // Artist wins even when AlbumArtist and Performer appear after it.
        let mut with_artist = CollectedTags::default();
        collect_standard_tags(
            &[
                std_tag(StandardTag::TrackTitle(Arc::new("  CANTO DE LUNA  ".to_string()))),
                std_tag(StandardTag::Artist(Arc::new("h6itam".to_string()))),
                std_tag(StandardTag::AlbumArtist(Arc::new("Album Artist".to_string()))),
                std_tag(StandardTag::Performer(Arc::new("Performer".to_string()))),
                std_tag(StandardTag::Album(Arc::new("Singles".to_string()))),
            ],
            &mut with_artist,
        );
        assert_eq!(with_artist.title.as_deref(), Some("CANTO DE LUNA"));
        assert_eq!(with_artist.album.as_deref(), Some("Singles"));
        assert_eq!(with_artist.resolved_artist().as_deref(), Some("h6itam"));

        // AlbumArtist is used when Artist is absent or blank, beating Performer.
        let mut with_album_artist = CollectedTags::default();
        collect_standard_tags(
            &[
                std_tag(StandardTag::Artist(Arc::new("   ".to_string()))),
                std_tag(StandardTag::Performer(Arc::new("Performer".to_string()))),
                std_tag(StandardTag::AlbumArtist(Arc::new("Album Artist".to_string()))),
            ],
            &mut with_album_artist,
        );
        assert_eq!(
            with_album_artist.resolved_artist().as_deref(),
            Some("Album Artist")
        );

        // Performer is used when both Artist and AlbumArtist are absent.
        let mut with_performer = CollectedTags::default();
        collect_standard_tags(
            &[std_tag(StandardTag::Performer(Arc::new("Session Performer".to_string())))],
            &mut with_performer,
        );
        assert_eq!(
            with_performer.resolved_artist().as_deref(),
            Some("Session Performer")
        );
    }

    #[test]
    fn volume_to_gain_maps_cubic_curve_and_clamps() {
        assert_eq!(volume_to_gain(0), 0.0);
        assert!((volume_to_gain(50) - 0.125).abs() < 1e-6);
        assert_eq!(volume_to_gain(100), 1.0);
        assert_eq!(volume_to_gain(101), 1.0);
        assert_eq!(volume_to_gain(255), 1.0);

        let mut prev = volume_to_gain(0);
        for vol in 1..=100 {
            let g = volume_to_gain(vol);
            assert!(
                g > prev,
                "volume_to_gain must strictly increase on 0..=100 (vol={vol}: {g} <= {prev})"
            );
            prev = g;
        }
        for vol in 101..=255 {
            assert_eq!(volume_to_gain(vol), 1.0);
        }
    }

    #[test]
    fn scale_pcm_f32_ramped_handles_empty_and_steady_state() {
        let mut dst = vec![99.0; 8];
        let gain = scale_pcm_f32_ramped(&[], &mut dst, 2, 0.25, 0.75, 16);
        assert!(dst.is_empty());
        assert_eq!(gain, 0.75);

        // Steady-state when start == target.
        let src = [1.0, -0.5, 0.25, -1.0];
        let gain = scale_pcm_f32_ramped(&src, &mut dst, 2, 0.5, 0.5, 10);
        assert_eq!(gain, 0.5);
        assert_eq!(dst, vec![0.5, -0.25, 0.125, -0.5]);

        // Steady-state when ramp_frames == 0.
        let gain = scale_pcm_f32_ramped(&src, &mut dst, 2, 1.0, 0.25, 0);
        assert_eq!(gain, 0.25);
        assert_eq!(dst, vec![0.25, -0.125, 0.0625, -0.25]);

        // Zero silence.
        let gain = scale_pcm_f32_ramped(&src, &mut dst, 2, 0.0, 0.0, 4);
        assert_eq!(gain, 0.0);
        assert_eq!(dst, vec![0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn scale_pcm_f32_ramped_completes_ramp_within_chunk() {
        // 6 stereo frames, ramp over 4 frames from 0.0 to 1.0.
        let src = [1.0; 12];
        let mut dst = Vec::new();
        let final_gain = scale_pcm_f32_ramped(&src, &mut dst, 2, 0.0, 1.0, 4);
        assert_eq!(final_gain, 1.0);

        let expected = [
            0.25, 0.25, // frame 0
            0.50, 0.50, // frame 1
            0.75, 0.75, // frame 2
            1.00, 1.00, // frame 3
            1.00, 1.00, // frame 4 (tail)
            1.00, 1.00, // frame 5 (tail)
        ];
        assert_eq!(dst.len(), expected.len());
        for (a, b) in dst.iter().zip(expected.iter()) {
            assert!((a - b).abs() < 1e-6, "got {a}, expected {b}");
        }
    }

    #[test]
    fn scale_pcm_f32_ramped_continues_across_short_chunks() {
        // Total ramp = 4 frames from 0.0 to 1.0, split across two 2-frame stereo chunks.
        let src1 = [1.0, -1.0, 1.0, -1.0];
        let mut dst1 = Vec::new();
        let gain_after_1 = scale_pcm_f32_ramped(&src1, &mut dst1, 2, 0.0, 1.0, 4);
        assert!((gain_after_1 - 0.5).abs() < 1e-6, "got {gain_after_1}");
        assert_eq!(dst1, vec![0.25, -0.25, 0.5, -0.5]);

        // Second chunk finishes the remaining 2 frames of the 4-frame ramp.
        let src2 = [1.0, -1.0, 1.0, -1.0];
        let mut dst2 = Vec::new();
        let gain_after_2 = scale_pcm_f32_ramped(&src2, &mut dst2, 2, gain_after_1, 1.0, 2);
        assert_eq!(gain_after_2, 1.0);
        assert_eq!(dst2, vec![0.75, -0.75, 1.0, -1.0]);
    }
}
