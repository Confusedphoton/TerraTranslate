//! Modality ingestion helpers that operate before provider-specific encoding.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct SampledFrame {
    pub captured_at_ms: i64,
    pub digest: String,
    pub bytes: Vec<u8>,
}

/// Optional policy for reusing a previously accepted frame when a new frame is structurally
/// similar to it. A threshold of `1.0` requires identical samples; lower thresholds permit more
/// visual drift. `None` preserves the sampler's exact-deduplication behavior.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FrameCacheConfig {
    pub similarity_threshold: Option<f32>,
}

impl FrameCacheConfig {
    pub const fn disabled() -> Self {
        Self {
            similarity_threshold: None,
        }
    }

    pub const fn with_similarity_threshold(similarity_threshold: f32) -> Self {
        Self {
            similarity_threshold: Some(similarity_threshold),
        }
    }

    /// Return a safe threshold for a score in the inclusive `[0, 1]` range.
    pub fn normalized_similarity_threshold(self) -> Option<f32> {
        self.similarity_threshold
            .filter(|threshold| threshold.is_finite())
            .map(|threshold| threshold.clamp(0.0, 1.0))
    }
}

/// Compute a windowed structural similarity score for byte-valued samples.
///
/// This is the SSIM luminance/contrast-structure formula with an 11-sample window and an
/// 8-bit sample range. The function is deliberately independent of image decoding so it can be
/// reused by ingestion code; pixel-oriented callers should pass a canonical luminance buffer.
/// Invalid or differently sized buffers have no reusable structure and score `0.0`.
pub fn structural_similarity(first: &[u8], second: &[u8]) -> f32 {
    const WINDOW_SIZE: usize = 11;
    const SAMPLE_RANGE: f64 = 255.0;
    const C1: f64 = (0.01 * SAMPLE_RANGE) * (0.01 * SAMPLE_RANGE);
    const C2: f64 = (0.03 * SAMPLE_RANGE) * (0.03 * SAMPLE_RANGE);

    if first.is_empty() || first.len() != second.len() {
        return 0.0;
    }
    if first == second {
        return 1.0;
    }

    let mut total = 0.0;
    let mut windows = 0usize;
    for (first_window, second_window) in first.chunks(WINDOW_SIZE).zip(second.chunks(WINDOW_SIZE)) {
        let count = first_window.len() as f64;
        let first_mean = first_window
            .iter()
            .map(|sample| f64::from(*sample))
            .sum::<f64>()
            / count;
        let second_mean = second_window
            .iter()
            .map(|sample| f64::from(*sample))
            .sum::<f64>()
            / count;
        let (mut first_variance, mut second_variance, mut covariance) = (0.0, 0.0, 0.0);
        for (first_sample, second_sample) in first_window.iter().zip(second_window) {
            let first_delta = f64::from(*first_sample) - first_mean;
            let second_delta = f64::from(*second_sample) - second_mean;
            first_variance += first_delta * first_delta;
            second_variance += second_delta * second_delta;
            covariance += first_delta * second_delta;
        }
        first_variance /= count;
        second_variance /= count;
        covariance /= count;

        let luminance = (2.0 * first_mean * second_mean + C1)
            / (first_mean * first_mean + second_mean * second_mean + C1);
        let contrast_structure = (2.0 * covariance + C2) / (first_variance + second_variance + C2);
        total += luminance * contrast_structure;
        windows += 1;
    }

    (total / windows as f64).clamp(0.0, 1.0) as f32
}

/// Exact-frame deduplication and rate limiting. Pixel decoding remains in the PipeWire adapter;
/// this sampler treats its input as opaque model samples. The optional similarity policy can be
/// used when those samples are already a canonical pixel/luminance representation.
pub struct FrameSampler {
    minimum_interval_ms: i64,
    last_accepted_at_ms: Option<i64>,
    recent_digests: VecDeque<String>,
    recent_capacity: usize,
    cache_config: FrameCacheConfig,
    cached_frame: Option<SampledFrame>,
}

impl FrameSampler {
    pub fn new(minimum_interval_ms: i64, recent_capacity: usize) -> Self {
        Self::with_config(
            minimum_interval_ms,
            recent_capacity,
            FrameCacheConfig::disabled(),
        )
    }

    pub fn with_config(
        minimum_interval_ms: i64,
        recent_capacity: usize,
        cache_config: FrameCacheConfig,
    ) -> Self {
        Self {
            minimum_interval_ms: minimum_interval_ms.max(0),
            last_accepted_at_ms: None,
            recent_digests: VecDeque::with_capacity(recent_capacity),
            recent_capacity,
            cache_config,
            cached_frame: None,
        }
    }

    pub fn cache_config(&self) -> FrameCacheConfig {
        self.cache_config
    }

    pub fn set_cache_config(&mut self, cache_config: FrameCacheConfig) {
        if cache_config.normalized_similarity_threshold().is_none() {
            self.cached_frame = None;
        }
        self.cache_config = cache_config;
    }

    pub fn sample(&mut self, captured_at_ms: i64, bytes: Vec<u8>) -> Option<SampledFrame> {
        if self
            .last_accepted_at_ms
            .is_some_and(|last| captured_at_ms.saturating_sub(last) < self.minimum_interval_ms)
        {
            return None;
        }

        if let Some(threshold) = self.cache_config.normalized_similarity_threshold()
            && let Some(cached_frame) = &self.cached_frame
            && bytes.len() == cached_frame.bytes.len()
            && structural_similarity(&cached_frame.bytes, &bytes) >= threshold
        {
            self.last_accepted_at_ms = Some(captured_at_ms);
            return Some(SampledFrame {
                captured_at_ms,
                digest: cached_frame.digest.clone(),
                bytes: cached_frame.bytes.clone(),
            });
        }

        let digest = blake3::hash(&bytes).to_hex().to_string();
        if self.recent_digests.contains(&digest) {
            return None;
        }
        self.last_accepted_at_ms = Some(captured_at_ms);
        if self.recent_capacity > 0 {
            if self.recent_digests.len() == self.recent_capacity {
                self.recent_digests.pop_front();
            }
            self.recent_digests.push_back(digest.clone());
        }
        let frame = SampledFrame {
            captured_at_ms,
            digest,
            bytes,
        };
        if self
            .cache_config
            .normalized_similarity_threshold()
            .is_some()
        {
            self.cached_frame = Some(frame.clone());
        }
        Some(frame)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSegment {
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub samples: Vec<i16>,
}

/// Small energy-based speech segmenter for mono PCM. It does not transcribe audio; completed
/// segments are sent unchanged to audio-capable models.
pub struct VoiceSegmenter {
    sample_rate: u32,
    energy_threshold: f32,
    trailing_silence_samples: usize,
    active: Vec<i16>,
    active_started_at_ms: Option<i64>,
    silent_samples: usize,
}

impl VoiceSegmenter {
    pub fn new(sample_rate: u32, energy_threshold: f32, trailing_silence_ms: u32) -> Self {
        Self {
            sample_rate,
            energy_threshold: energy_threshold.max(0.0),
            trailing_silence_samples: (sample_rate as usize * trailing_silence_ms as usize) / 1000,
            active: Vec::new(),
            active_started_at_ms: None,
            silent_samples: 0,
        }
    }

    pub fn push_chunk(
        &mut self,
        chunk_started_at_ms: i64,
        samples: &[i16],
    ) -> Option<AudioSegment> {
        if samples.is_empty() {
            return None;
        }
        let rms = (samples
            .iter()
            .map(|sample| (*sample as f64) * (*sample as f64))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt() as f32;
        let voiced = rms >= self.energy_threshold;
        if voiced && self.active_started_at_ms.is_none() {
            self.active_started_at_ms = Some(chunk_started_at_ms);
        }
        if self.active_started_at_ms.is_some() {
            self.active.extend_from_slice(samples);
            if voiced {
                self.silent_samples = 0;
            } else {
                self.silent_samples += samples.len();
            }
            if self.silent_samples >= self.trailing_silence_samples {
                return self.take_segment(
                    chunk_started_at_ms + samples.len() as i64 * 1000 / self.sample_rate as i64,
                );
            }
        }
        None
    }

    pub fn flush(&mut self, ended_at_ms: i64) -> Option<AudioSegment> {
        self.take_segment(ended_at_ms)
    }

    fn take_segment(&mut self, ended_at_ms: i64) -> Option<AudioSegment> {
        let started_at_ms = self.active_started_at_ms.take()?;
        self.silent_samples = 0;
        Some(AudioSegment {
            started_at_ms,
            ended_at_ms,
            samples: std::mem::take(&mut self.active),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sampler_debounces_rate_and_duplicates() {
        let mut sampler = FrameSampler::new(100, 4);
        assert!(sampler.sample(0, vec![1]).is_some());
        assert!(sampler.sample(50, vec![2]).is_none());
        assert!(sampler.sample(100, vec![1]).is_none());
        assert!(sampler.sample(101, vec![2]).is_some());
    }

    #[test]
    fn structural_similarity_scores_identical_and_changed_samples() {
        assert_eq!(structural_similarity(&[10, 20, 30], &[10, 20, 30]), 1.0);
        assert!(structural_similarity(&[0; 32], &[255; 32]) < 0.01);
        assert!(
            structural_similarity(
                &[0; 32],
                &[
                    0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0
                ]
            ) > 0.9
        );
    }

    #[test]
    fn frame_sampler_reuses_a_similar_cached_frame() {
        let mut sampler =
            FrameSampler::with_config(0, 4, FrameCacheConfig::with_similarity_threshold(0.9));
        let original = sampler.sample(0, vec![40; 32]).unwrap();
        let reused = sampler
            .sample(100, {
                let mut bytes = vec![40; 32];
                bytes[5] = 41;
                bytes
            })
            .unwrap();

        assert_eq!(reused.captured_at_ms, 100);
        assert_eq!(reused.digest, original.digest);
        assert_eq!(reused.bytes, original.bytes);
    }

    #[test]
    fn frame_sampler_replaces_the_cache_for_a_dissimilar_frame() {
        let mut sampler =
            FrameSampler::with_config(0, 4, FrameCacheConfig::with_similarity_threshold(0.9));
        let original = sampler.sample(0, vec![0; 32]).unwrap();
        let replacement = sampler.sample(100, vec![255; 32]).unwrap();

        assert_ne!(replacement.digest, original.digest);
        assert_eq!(replacement.bytes, vec![255; 32]);
        let reused = sampler.sample(200, vec![254; 32]).unwrap();
        assert_eq!(reused.digest, replacement.digest);
        assert_eq!(reused.bytes, replacement.bytes);
    }

    #[test]
    fn voice_segmenter_closes_after_silence() {
        let mut segmenter = VoiceSegmenter::new(1_000, 100.0, 10);
        assert!(segmenter.push_chunk(0, &[1_000; 10]).is_none());
        let segment = segmenter.push_chunk(10, &[0; 10]).unwrap();
        assert_eq!(segment.started_at_ms, 0);
        assert_eq!(segment.ended_at_ms, 20);
        assert_eq!(segment.samples.len(), 20);
    }
}
