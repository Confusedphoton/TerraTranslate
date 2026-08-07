//! Modality ingestion helpers that operate before provider-specific encoding.

use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct SampledFrame {
    pub captured_at_ms: i64,
    pub digest: String,
    pub bytes: Vec<u8>,
}

/// Exact-frame deduplication and rate limiting. Pixel decoding remains in the PipeWire adapter;
/// this sampler deliberately treats encoded frame bytes as opaque model input.
pub struct FrameSampler {
    minimum_interval_ms: i64,
    last_accepted_at_ms: Option<i64>,
    recent_digests: VecDeque<String>,
    recent_capacity: usize,
}

impl FrameSampler {
    pub fn new(minimum_interval_ms: i64, recent_capacity: usize) -> Self {
        Self {
            minimum_interval_ms: minimum_interval_ms.max(0),
            last_accepted_at_ms: None,
            recent_digests: VecDeque::with_capacity(recent_capacity),
            recent_capacity,
        }
    }

    pub fn sample(&mut self, captured_at_ms: i64, bytes: Vec<u8>) -> Option<SampledFrame> {
        if self
            .last_accepted_at_ms
            .is_some_and(|last| captured_at_ms.saturating_sub(last) < self.minimum_interval_ms)
        {
            return None;
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
        Some(SampledFrame {
            captured_at_ms,
            digest,
            bytes,
        })
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
    fn voice_segmenter_closes_after_silence() {
        let mut segmenter = VoiceSegmenter::new(1_000, 100.0, 10);
        assert!(segmenter.push_chunk(0, &[1_000; 10]).is_none());
        let segment = segmenter.push_chunk(10, &[0; 10]).unwrap();
        assert_eq!(segment.started_at_ms, 0);
        assert_eq!(segment.ended_at_ms, 20);
        assert_eq!(segment.samples.len(), 20);
    }
}
