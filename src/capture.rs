//! System-wide audio capture via ScreenCaptureKit (macOS). Delivers 16 kHz mono f32.

use screencapturekit::prelude::*;
use std::sync::mpsc::Sender;

pub const SAMPLE_RATE: u32 = 16_000;

struct AudioOut {
    tx: Sender<Vec<f32>>,
}

impl SCStreamOutputTrait for AudioOut {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if !matches!(output_type, SCStreamOutputType::Audio) {
            return;
        }
        let Ok(list) = sample.audio_buffer_list() else {
            return;
        };
        let to_f32 = |b: &[u8]| -> Vec<f32> {
            b.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        };
        let n = list.num_buffers();
        let mono: Vec<f32> = if n >= 2 {
            // planar: one buffer per channel
            let chans: Vec<Vec<f32>> = list.iter().map(|b| to_f32(b.data())).collect();
            let len = chans.iter().map(Vec::len).min().unwrap_or(0);
            (0..len)
                .map(|i| chans.iter().map(|c| c[i]).sum::<f32>() / chans.len() as f32)
                .collect()
        } else if let Some(b) = list.buffer(0) {
            let ch = (list.get(0).map_or(1, |x| x.number_channels) as usize).max(1);
            to_f32(b.data())
                .chunks_exact(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32)
                .collect()
        } else {
            return;
        };
        if !mono.is_empty() {
            let _ = self.tx.send(mono);
        }
    }
}

pub struct Capture {
    stream: SCStream,
}

impl Capture {
    /// Starts capturing every sound the Mac plays. Fails when Screen Recording is not granted.
    pub fn start(tx: Sender<Vec<f32>>) -> Result<Self, String> {
        let content = SCShareableContent::get().map_err(|e| format!("画面収録の許可が必要: {e}"))?;
        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or("ディスプレイが見つからない")?;
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build()
            .map_err(|e| e.to_string())?;
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_excludes_current_process_audio(true)
            .with_sample_rate(SAMPLE_RATE as i32)
            .with_channel_count(1);
        let mut stream = SCStream::new(&filter, &config).map_err(|e| e.to_string())?;
        stream
            .add_output_handler(AudioOut { tx }, SCStreamOutputType::Audio)
            .map_err(|e| e.to_string())?;
        stream.start_capture().map_err(|e| e.to_string())?;
        Ok(Self { stream })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}
