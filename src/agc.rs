//! Automatic gain control for the captured system audio.
//!
//! The Mac hands us the pre-output digital signal, which is often very quiet. The AGC follows a
//! fast-attack / slow-release loudness envelope and steers the gain toward a fixed target level.
//! It holds the gain while the input is near-silent so background noise is not pumped up.

use crate::capture::SAMPLE_RATE;

const TARGET_RMS: f32 = 0.1; // about -20 dBFS
const MAX_GAIN: f32 = 60.0; // about +36 dB
const MIN_GAIN: f32 = 1.0; // never attenuates quiet-to-normal material
const GATE_RMS: f32 = 0.0008; // below this the envelope and gain are frozen
const ATTACK_SECONDS: f32 = 0.03;
const RELEASE_SECONDS: f32 = 3.0;

pub struct Agc {
    envelope: f32,
    gain: f32,
}

impl Agc {
    pub fn new() -> Self {
        Self { envelope: 0.0, gain: MIN_GAIN }
    }

    pub fn gain_db(&self) -> f32 {
        20.0 * self.gain.log10()
    }

    /// Applies the gain to `chunk` in place and returns the RMS of the leveled chunk.
    pub fn process(&mut self, chunk: &mut [f32]) -> f32 {
        if chunk.is_empty() {
            return 0.0;
        }
        let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
        let previous = self.gain;
        if rms > GATE_RMS {
            let seconds = chunk.len() as f32 / SAMPLE_RATE as f32;
            let tau = if rms > self.envelope { ATTACK_SECONDS } else { RELEASE_SECONDS };
            self.envelope += (rms - self.envelope) * (1.0 - (-seconds / tau).exp());
            self.gain = (TARGET_RMS / self.envelope.max(GATE_RMS)).clamp(MIN_GAIN, MAX_GAIN);
        }
        // Ramp across the chunk so a gain change does not click.
        let step = (self.gain - previous) / chunk.len() as f32;
        let mut g = previous;
        let mut sum = 0.0;
        for s in chunk.iter_mut() {
            g += step;
            *s = (*s * g).clamp(-1.0, 1.0);
            sum += *s * *s;
        }
        (sum / chunk.len() as f32).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amplitude: f32, len: usize) -> Vec<f32> {
        (0..len).map(|i| amplitude * (i as f32 * 0.3).sin()).collect()
    }

    #[test]
    fn quiet_input_converges_to_target_and_silence_holds_gain() {
        let mut agc = Agc::new();
        let chunk = SAMPLE_RATE as usize / 50;
        let mut level = 0.0;
        for _ in 0..500 {
            level = agc.process(&mut tone(0.007, chunk)); // 10 s of quiet speech-like input
        }
        assert!((level - TARGET_RMS).abs() < 0.03, "leveled rms {level}");
        let held = agc.gain_db();
        for _ in 0..500 {
            agc.process(&mut vec![0.0; chunk]);
        }
        assert_eq!(agc.gain_db(), held, "gain must be frozen during silence");
    }
}
