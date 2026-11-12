//! Lightweight pure-Rust PCM channel remixing and linear sample-rate adaptation.
//!
//! Used by platform backends (such as AAudio on Android) whose negotiated device
//! stream format can differ from Symphonia's decoded `(sample_rate, channels)`.

use crate::error::{Result, TermusicError};

/// Adapts interleaved `f32` PCM from source `(sample_rate, channels)` to destination
/// `(sample_rate, channels)` with streaming continuity across consecutive chunks.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PcmAdapter {
    src_rate: u32,
    src_channels: usize,
    dst_rate: u32,
    dst_channels: usize,
    /// Interleaved samples already remixed to `dst_channels` awaiting interpolation.
    stage: Vec<f32>,
    /// Fractional frame cursor within `stage`.
    pos: f64,
}

#[allow(dead_code)]
impl PcmAdapter {
    /// Creates a new adapter, validating non-zero rates and channel counts.
    pub fn new(src_rate: u32, src_channels: usize, dst_rate: u32, dst_channels: usize) -> Result<Self> {
        if src_rate == 0 || dst_rate == 0 {
            return Err(TermusicError::Audio(format!(
                "invalid sample rate for PCM adapter: src={src_rate}, dst={dst_rate}"
            )));
        }
        if src_channels == 0 || dst_channels == 0 {
            return Err(TermusicError::Audio(format!(
                "invalid channel count for PCM adapter: src={src_channels}, dst={dst_channels}"
            )));
        }
        Ok(Self {
            src_rate,
            src_channels,
            dst_rate,
            dst_channels,
            stage: Vec::new(),
            pos: 0.0,
        })
    }

    /// Returns `true` when source and destination formats match bit-for-bit.
    pub fn is_passthrough(&self) -> bool {
        self.src_rate == self.dst_rate && self.src_channels == self.dst_channels
    }

    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    pub fn src_channels(&self) -> usize {
        self.src_channels
    }

    pub fn dst_rate(&self) -> u32 {
        self.dst_rate
    }

    pub fn dst_channels(&self) -> usize {
        self.dst_channels
    }

    /// Discards any buffered resampler state (call on flush/seek).
    pub fn reset(&mut self) {
        self.stage.clear();
        self.pos = 0.0;
    }

    /// Adapts `input` (interleaved `src_channels`) into `out` (interleaved `dst_channels`).
    ///
    /// `out` is cleared before writing. Returns an error if `input.len()` is not a
    /// whole number of `src_channels`-wide frames.
    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        out.clear();
        if input.is_empty() {
            return Ok(());
        }
        if input.len() % self.src_channels != 0 {
            return Err(TermusicError::Audio(format!(
                "PCM sample slice length {} is not divisible by {} source channels",
                input.len(),
                self.src_channels
            )));
        }

        if self.is_passthrough() {
            out.extend_from_slice(input);
            return Ok(());
        }

        let in_frames = input.len() / self.src_channels;
        if self.src_rate == self.dst_rate {
            out.resize(in_frames * self.dst_channels, 0.0);
            for frame in 0..in_frames {
                let s_off = frame * self.src_channels;
                let d_off = frame * self.dst_channels;
                remix_frame(
                    &input[s_off..s_off + self.src_channels],
                    &mut out[d_off..d_off + self.dst_channels],
                );
            }
            return Ok(());
        }

        let old_len = self.stage.len();
        self.stage.resize(old_len + in_frames * self.dst_channels, 0.0);
        for frame in 0..in_frames {
            let s_off = frame * self.src_channels;
            let d_off = old_len + frame * self.dst_channels;
            remix_frame(
                &input[s_off..s_off + self.src_channels],
                &mut self.stage[d_off..d_off + self.dst_channels],
            );
        }

        let stage_frames = self.stage.len() / self.dst_channels;
        let step = self.src_rate as f64 / self.dst_rate as f64;
        let est_out_frames = ((stage_frames as f64) / step).ceil() as usize + 1;
        out.reserve(est_out_frames * self.dst_channels);

        loop {
            let i0 = self.pos.floor() as usize;
            let i1 = i0 + 1;
            if i1 >= stage_frames {
                break;
            }
            let frac = (self.pos - i0 as f64) as f32;
            let base0 = i0 * self.dst_channels;
            let base1 = i1 * self.dst_channels;
            for ch in 0..self.dst_channels {
                let a = self.stage[base0 + ch];
                let b = self.stage[base1 + ch];
                out.push(a + (b - a) * frac);
            }
            self.pos += step;
        }

        let consumed = (self.pos.floor() as usize).min(stage_frames);
        if consumed > 0 {
            self.stage.drain(..consumed * self.dst_channels);
            self.pos -= consumed as f64;
        }

        Ok(())
    }

    /// Emits any final sub-frame sample remaining in the resampler staging buffer at EOF.
    pub fn flush_tail(&mut self, out: &mut Vec<f32>) {
        out.clear();
        let stage_frames = self.stage.len() / self.dst_channels;
        let idx = self.pos.floor() as usize;
        if idx < stage_frames {
            let base = idx * self.dst_channels;
            out.extend_from_slice(&self.stage[base..base + self.dst_channels]);
        }
        self.reset();
    }
}

fn remix_frame(src: &[f32], dst: &mut [f32]) {
    let src_ch = src.len();
    let dst_ch = dst.len();
    if src_ch == dst_ch {
        dst.copy_from_slice(src);
        return;
    }

    if dst_ch == 1 {
        let sum: f32 = src.iter().copied().sum();
        dst[0] = sum / src_ch as f32;
        return;
    }

    if src_ch == 1 {
        dst[0] = src[0];
        dst[1] = src[0];
        dst[2..].fill(0.0);
        return;
    }

    if dst_ch == 2 {
        // src_ch >= 3: preserve FL/FR and fold common center/surround channels smoothly.
        let mut left = src[0];
        let mut right = src[1];
        if src_ch == 3 {
            // [L, R, C]
            let c = src[2] * core::f32::consts::FRAC_1_SQRT_2;
            left += c;
            right += c;
        } else if src_ch == 4 {
            // [L, R, Ls, Rs]
            left += src[2] * core::f32::consts::FRAC_1_SQRT_2;
            right += src[3] * core::f32::consts::FRAC_1_SQRT_2;
        } else {
            // 5.1 / 7.1 style layouts: [L, R, C, LFE, Ls, Rs, (Sl, Sr)]
            let c = src[2] * core::f32::consts::FRAC_1_SQRT_2;
            left += c;
            right += c;
            if src_ch >= 6 {
                left += src[4] * core::f32::consts::FRAC_1_SQRT_2;
                right += src[5] * core::f32::consts::FRAC_1_SQRT_2;
            }
            if src_ch >= 8 {
                left += src[6] * 0.5;
                right += src[7] * 0.5;
            }
        }
        dst[0] = left.clamp(-1.0, 1.0);
        dst[1] = right.clamp(-1.0, 1.0);
        return;
    }

    // General M -> N fallback when dst_ch > 2: copy overlapping channels and zero pad.
    let copy = src_ch.min(dst_ch);
    dst[..copy].copy_from_slice(&src[..copy]);
    dst[copy..].fill(0.0);
}

#[cfg(test)]
mod tests {
    use super::PcmAdapter;

    #[test]
    fn validates_zero_rates_channels_and_misaligned_input() {
        assert!(PcmAdapter::new(0, 2, 48_000, 2).is_err());
        assert!(PcmAdapter::new(44_100, 0, 48_000, 2).is_err());
        assert!(PcmAdapter::new(44_100, 2, 0, 2).is_err());
        assert!(PcmAdapter::new(44_100, 2, 48_000, 0).is_err());

        let mut adapter = PcmAdapter::new(44_100, 2, 44_100, 2).unwrap();
        assert!(adapter.is_passthrough());
        let mut out = Vec::new();
        assert!(adapter.process_into(&[0.1, 0.2, 0.3], &mut out).is_err());
    }

    #[test]
    fn remixes_mono_to_stereo_and_surround_to_stereo() {
        let mut mono_to_stereo = PcmAdapter::new(48_000, 1, 48_000, 2).unwrap();
        assert!(!mono_to_stereo.is_passthrough());
        let mut out = Vec::new();
        mono_to_stereo.process_into(&[0.25, -0.5], &mut out).unwrap();
        assert_eq!(out, vec![0.25, 0.25, -0.5, -0.5]);

        let mut stereo_to_mono = PcmAdapter::new(48_000, 2, 48_000, 1).unwrap();
        stereo_to_mono.process_into(&[0.2, 0.6, -0.4, 0.4], &mut out).unwrap();
        assert!((out[0] - 0.4).abs() < 1e-6);
        assert!(out[1].abs() < 1e-6);

        let mut six_to_two = PcmAdapter::new(48_000, 6, 48_000, 2).unwrap();
        // L=0.2, R=0.3, C=0.0, LFE=0.9 (ignored in stereo fold), Ls=0.0, Rs=0.0
        six_to_two
            .process_into(&[0.2, 0.3, 0.0, 0.9, 0.0, 0.0], &mut out)
            .unwrap();
        assert!((out[0] - 0.2).abs() < 1e-6);
        assert!((out[1] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn resamples_dc_and_linear_ramp_continuously_across_chunks() {
        let mut adapter = PcmAdapter::new(44_100, 2, 48_000, 2).unwrap();
        let dc_chunk: Vec<f32> = std::iter::repeat([0.5_f32, -0.25_f32])
            .take(512)
            .flatten()
            .collect();
        let mut out = Vec::new();
        adapter.process_into(&dc_chunk, &mut out).unwrap();
        assert!(!out.is_empty());
        for pair in out.chunks_exact(2) {
            assert!((pair[0] - 0.5).abs() < 1e-5);
            assert!((pair[1] + 0.25).abs() < 1e-5);
        }

        // Verify linear ramp continuity across chunk split.
        let mut whole = PcmAdapter::new(24_000, 1, 48_000, 1).unwrap();
        let mut split = PcmAdapter::new(24_000, 1, 48_000, 1).unwrap();
        let ramp: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();

        let mut whole_out = Vec::new();
        whole.process_into(&ramp, &mut whole_out).unwrap();
        let mut tail = Vec::new();
        whole.flush_tail(&mut tail);
        whole_out.extend_from_slice(&tail);

        let mut split_out = Vec::new();
        let mut tmp = Vec::new();
        split.process_into(&ramp[..37], &mut tmp).unwrap();
        split_out.extend_from_slice(&tmp);
        split.process_into(&ramp[37..], &mut tmp).unwrap();
        split_out.extend_from_slice(&tmp);
        split.flush_tail(&mut tail);
        split_out.extend_from_slice(&tail);

        assert_eq!(whole_out.len(), split_out.len());
        for (a, b) in whole_out.iter().zip(split_out.iter()) {
            assert!((a - b).abs() < 1e-5, "mismatch: {a} vs {b}");
        }

        split.reset();
        split.process_into(&[], &mut tmp).unwrap();
        assert!(tmp.is_empty());
    }
}
