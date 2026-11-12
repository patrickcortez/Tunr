use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, MediaInfo, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag, Tag};
use symphonia::core::units::{Duration, Time, TimeBase};

use crate::error::{Result, TermusicError};

pub mod null_sink;
pub mod pulse;

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
/// Sinks are [`Send`] so ownership can be transferred into the dedicated `termusic-engine` OS
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

/// Owned chunk of decoded interleaved `f32` PCM samples with its presentation timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedChunk {
    pub samples: Vec<f32>,
    pub position_secs: f64,
}

/// Demuxer and decoder state for a single audio track opened via Symphonia.
///
/// Intended for single-owner `&mut self` decoding on the `termusic-engine` playback thread;
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
            .ok_or_else(|| TermusicError::Audio(format!("no audio track found in '{}'", path.display())))?;
        let id = track.id;
        let track_dur = compute_track_duration_secs(track);
        (id, track_dur)
    };
    let duration_secs = duration_secs.or_else(|| compute_media_duration_secs(format.media_info()));

    let mut title = None;
    let mut artist = None;
    let mut album = None;

    let mut meta = format.metadata();
    loop {
        if let Some(rev) = meta.current() {
            collect_standard_tags(&rev.media.tags, &mut title, &mut artist, &mut album);
            for per_track in &rev.per_track {
                if per_track.track_id == u64::from(track_id) {
                    collect_standard_tags(
                        &per_track.metadata.tags,
                        &mut title,
                        &mut artist,
                        &mut album,
                    );
                }
            }
        }
        if meta.pop().is_none() {
            break;
        }
    }

    Ok(TrackProbe {
        title,
        artist,
        album,
        duration_secs,
    })
}

impl SymphoniaTrack {
    /// Open `path`, probe its container format, and initialize an audio decoder for the default
    /// audio track.
    pub fn open(path: &Path) -> Result<Self> {
        let format = probe_format(path)?;
        let (track_id, time_base, audio_params, track_dur) = {
            let track = format
                .default_track(TrackType::Audio)
                .ok_or_else(|| TermusicError::Audio(format!("no audio track found in '{}'", path.display())))?;
            let audio_params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or_else(|| {
                    TermusicError::Audio(format!(
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
            return Err(TermusicError::InvalidArg(
                "seek target must be a finite number of seconds".to_string(),
            ));
        }

        let clamped = target_secs.max(0.0);
        let time = Time::try_from_secs_f64(clamped).ok_or_else(|| {
            TermusicError::Audio(format!("seek target out of representable range: {clamped}"))
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
                .ok_or_else(|| TermusicError::Audio("no audio track after stream reset".to_string()))?;
            let audio_params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or_else(|| {
                    TermusicError::Audio("missing audio codec parameters after stream reset".to_string())
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
        .ok_or_else(|| TermusicError::Audio("missing or zero audio sample rate".to_string()))?;

    let ch_count = track_params
        .channels
        .as_ref()
        .or(decoder_params.channels.as_ref())
        .map(|c| c.count())
        .filter(|&c| c > 0)
        .ok_or_else(|| TermusicError::Audio("missing or zero audio channel count".to_string()))?;

    let channels = u8::try_from(ch_count).map_err(|_| {
        TermusicError::Audio(format!("unsupported channel count {ch_count} (exceeds u8::MAX)"))
    })?;

    Ok((sample_rate, channels))
}

fn collect_standard_tags(
    tags: &[Tag],
    title: &mut Option<String>,
    artist: &mut Option<String>,
    album: &mut Option<String>,
) {
    for tag in tags {
        let Some(std_tag) = &tag.std else {
            continue;
        };
        match std_tag {
            StandardTag::TrackTitle(val) => set_non_empty(title, val),
            StandardTag::Artist(val) => set_non_empty(artist, val),
            StandardTag::Album(val) => set_non_empty(album, val),
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

fn map_symphonia_err(context: &str, err: SymError) -> TermusicError {
    match err {
        SymError::IoError(io_err) => TermusicError::Io(io_err),
        other => TermusicError::Audio(format!("{context}: {other}")),
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
            "termusic-audio-test-{}-{idx}",
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
}
