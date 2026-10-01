//! capture → utterance segmentation → whisper → translation, each stage on its own thread.

use crate::agc::Agc;
use crate::capture::{Capture, SAMPLE_RATE};
use crate::translate::{self, Engine, TranslateSettings};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Loading,
    Ready,
    Idle,
}

pub enum Event {
    /// Speech recognition model state.
    Asr(Stage),
    /// Translation model state (Ollama warm-up).
    Translator(Stage),
    /// RMS after automatic gain control, and the gain currently applied.
    Level { rms: f32, gain_db: f32 },
    Heard { id: u64, lang: String, text: String },
    Translated { id: u64, text: String },
    TranslateFailed { id: u64, error: String },
    Fatal(String),
}

pub type SharedSettings = Arc<Mutex<TranslateSettings>>;

/// Subtitle ids keep counting across stop and start: the lines of an earlier run stay on screen, and a late
/// translation must not land on a newer line that was given the same id.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// The level meter is fed at most this often; captured chunks arrive faster than a meter can show.
const METER_INTERVAL: Duration = Duration::from_millis(50);

pub struct Pipeline {
    _capture: Capture,
    stop: Arc<AtomicBool>,
}

/// The running recognition thread and its stop flag, so the process can wait for it before it exits.
static ASR: Mutex<Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>> = Mutex::new(None);

/// Stops the recognition thread and waits (at most a few seconds) until it has freed the whisper model.
///
/// The Metal backend frees its device when the process exits and aborts if a model is still alive then, which
/// showed up as a crash report on every quit. A model that is not freed in time is left to the OS.
pub fn finish_asr() {
    let running = ASR.lock().ok().and_then(|mut r| r.take());
    let Some((stop, handle)) = running else {
        return;
    };
    stop.store(true, Ordering::Relaxed);
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        let _ = handle.join();
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn model_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LIVE_SUBTITLE_MODEL") {
        return p.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/LiveSubtitle/ggml-large-v3-turbo-q5_0.bin")
}

impl Pipeline {
    /// `meter` says whether the level meter is on screen; while it is not, no level is reported and the window
    /// is not redrawn for it.
    pub fn start(
        settings: SharedSettings,
        tx: Sender<Event>,
        repaint: impl Fn() + Send + Sync + Clone + 'static,
        meter: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let model = model_path();
        if !model.is_file() {
            return Err(format!(
                "whisper のモデルが無い: {}（LIVE_SUBTITLE_MODEL で場所を指定できる）",
                model.display()
            ));
        }
        let (audio_tx, audio_rx) = mpsc::channel();
        let capture = Capture::start(audio_tx)?;
        let stop = Arc::new(AtomicBool::new(false));

        let _ = tx.send(Event::Asr(Stage::Loading));
        let engine = settings.lock().map(|s| s.engine).unwrap_or(Engine::Off);
        if matches!(engine, Engine::Ollama | Engine::Claude | Engine::Codex) {
            warm_up(settings.clone(), tx.clone(), repaint.clone());
        } else {
            let _ = tx.send(Event::Translator(Stage::Idle));
        }

        // The leveler runs apart from whisper so the meter keeps moving while a decode is in progress.
        let (leveled_tx, leveled_rx) = mpsc::channel();
        {
            let (tx, repaint, stop) = (tx.clone(), repaint.clone(), stop.clone());
            std::thread::spawn(move || run_leveler(audio_rx, leveled_tx, &tx, &repaint, &stop, &meter));
        }

        let stop_asr = stop.clone();
        let handle = std::thread::spawn(move || {
            if let Err(e) = run_asr(leveled_rx, &model, settings, &tx, &repaint, &stop_asr) {
                let _ = tx.send(Event::Fatal(e));
                repaint();
            }
        });
        if let Ok(mut running) = ASR.lock() {
            *running = Some((stop.clone(), handle));
        }
        Ok(Self { _capture: capture, stop })
    }
}

/// Loads the translation model (Ollama) or starts the Claude session so the first subtitle is not delayed by a cold start.
pub fn warm_up(
    settings: SharedSettings,
    tx: Sender<Event>,
    repaint: impl Fn() + Send + Sync + 'static,
) {
    let _ = tx.send(Event::Translator(Stage::Loading));
    repaint();
    std::thread::spawn(move || {
        let s = settings.lock().map(|s| s.clone()).unwrap_or_default();
        translate::warm_up(&s);
        let _ = tx.send(Event::Translator(Stage::Ready));
        repaint();
    });
}

/// System audio often arrives quiet, so the absolute floor for "voiced" is low and the real test is relative.
const MIN_VOICED_RMS: f32 = 0.0025;
const FRAME: usize = SAMPLE_RATE as usize / 50; // 20 ms
const PRE_ROLL: usize = 15; // 300 ms kept before the first voiced frame
const END_SILENCE: usize = 25; // 500 ms of quiet ends an utterance
const KEEP_TAIL: usize = 10; // quiet frames kept at the end of an utterance
const MIN_SPEECH: usize = 15; // shorter bursts are noise
const MAX_FRAMES: usize = 500; // 10 s hard cap so continuous audio still produces subtitles
const CUT_WINDOW: usize = 150; // the cap cuts at the quietest frame within the last 3 s
const FLOOR_FALL: f32 = 0.2; // the floor drops to a quieter frame within a few frames
const FLOOR_RISE: f32 = 0.002; // and creeps up toward a louder level over about ten seconds

/// Energy-based utterance segmenter.
///
/// Voice activity is judged on the raw signal; the audio kept for whisper is the AGC-leveled twin.
struct Segmenter {
    carry: Vec<f32>,
    carry_leveled: Vec<f32>,
    frames: Vec<f32>,
    rms: Vec<f32>,
    in_speech: bool,
    speech: usize,
    silence: usize,
    floor: f32,
}

impl Segmenter {
    fn new() -> Self {
        Self {
            carry: Vec::new(),
            carry_leveled: Vec::new(),
            frames: Vec::new(),
            rms: Vec::new(),
            in_speech: false,
            speech: 0,
            silence: 0,
            floor: 0.002,
        }
    }

    fn debug(&self) -> String {
        format!(
            "in_speech={} speech={} silence={} frames={} floor={:.4}",
            self.in_speech, self.speech, self.silence, self.rms.len(), self.floor
        )
    }

    fn reset(&mut self) {
        self.frames.clear();
        self.rms.clear();
        self.in_speech = false;
        self.speech = 0;
        self.silence = 0;
    }

    fn push(&mut self, raw: &[f32], leveled: &[f32]) -> Vec<Vec<f32>> {
        self.carry.extend_from_slice(raw);
        self.carry_leveled.extend_from_slice(leveled);
        let mut out = Vec::new();
        while self.carry.len() >= FRAME {
            let raw_frame: Vec<f32> = self.carry.drain(..FRAME).collect();
            let frame: Vec<f32> = self.carry_leveled.drain(..FRAME).collect();
            let e = (raw_frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
            let voiced = e > (self.floor * 2.5).max(MIN_VOICED_RMS);
            // The floor follows the quietest recent level: the background between words. Averaging every
            // unvoiced frame instead lets speech that is only a little above a steady background (wind, a
            // crowd, music) pull the floor up to its own level, after which nothing counts as voiced.
            self.floor += (e - self.floor) * if e < self.floor { FLOOR_FALL } else { FLOOR_RISE };
            self.frames.extend_from_slice(&frame);
            self.rms.push(e);
            if voiced {
                self.speech += 1;
                self.silence = 0;
                self.in_speech = true;
            } else if self.in_speech {
                self.silence += 1;
            }
            if !self.in_speech {
                if self.rms.len() > PRE_ROLL {
                    self.frames.drain(..FRAME);
                    self.rms.remove(0);
                }
                continue;
            }
            if self.silence >= END_SILENCE {
                let end = self.frames.len() - self.silence.saturating_sub(KEEP_TAIL) * FRAME;
                let enough = self.speech >= MIN_SPEECH;
                let utterance = self.frames[..end].to_vec();
                self.reset();
                if enough {
                    out.push(utterance);
                }
            } else if self.rms.len() >= MAX_FRAMES {
                let from = self.rms.len() - CUT_WINDOW;
                let cut = from
                    + self.rms[from..]
                        .iter()
                        .enumerate()
                        .min_by(|a, b| a.1.total_cmp(b.1))
                        .map_or(0, |(i, _)| i);
                out.push(self.frames[..cut * FRAME].to_vec());
                self.frames.drain(..cut * FRAME);
                self.rms.drain(..cut);
                self.speech = self.rms.len();
                self.silence = 0;
            }
        }
        out
    }
}

/// Appends a line to the file named by LIVE_SUBTITLE_DEBUG_LOG (diagnostics only).
fn debug_log(line: &str) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("LIVE_SUBTITLE_DEBUG_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn is_noise(text: &str) -> bool {
    text.chars().all(|c| !c.is_alphanumeric())
        || (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'))
}

/// Applies AGC to every captured chunk as it arrives and reports the level for the meter.
fn run_leveler(
    rx: Receiver<Vec<f32>>,
    out: Sender<(Vec<f32>, Vec<f32>)>,
    tx: &Sender<Event>,
    repaint: &(impl Fn() + Send + Sync),
    stop: &AtomicBool,
    meter: &AtomicBool,
) {
    let mut agc = Agc::new();
    let (mut chunks, mut peak, mut tick) = (0usize, 0f32, std::time::Instant::now());
    let (mut meter_peak, mut meter_tick) = (0f32, std::time::Instant::now());
    while !stop.load(Ordering::Relaxed) {
        let raw = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut leveled = raw.clone();
        let rms = agc.process(&mut leveled);
        meter_peak = meter_peak.max(rms);
        if meter_tick.elapsed() >= METER_INTERVAL {
            if meter.load(Ordering::Relaxed) {
                let _ = tx.send(Event::Level { rms: meter_peak, gain_db: agc.gain_db() });
                repaint();
            }
            (meter_peak, meter_tick) = (0.0, std::time::Instant::now());
        }
        chunks += 1;
        peak = peak.max(rms);
        if tick.elapsed() >= Duration::from_secs(2) {
            debug_log(&format!("chunks={chunks} leveled_peak_rms={peak:.4} agc_gain={:.1}dB", agc.gain_db()));
            (chunks, peak, tick) = (0, 0.0, std::time::Instant::now());
        }
        if out.send((raw, leveled)).is_err() {
            break;
        }
    }
}

fn run_asr(
    rx: Receiver<(Vec<f32>, Vec<f32>)>,
    model: &PathBuf,
    settings: SharedSettings,
    tx: &Sender<Event>,
    repaint: &(impl Fn() + Send + Sync + Clone + 'static),
    stop: &AtomicBool,
) -> Result<(), String> {
    // whisper-rs switches flash attention off by default; whisper.cpp itself has it on, and it is faster on Metal.
    let mut context_params = WhisperContextParameters::default();
    context_params.flash_attn(true);
    let ctx = WhisperContext::new_with_params(model.to_str().ok_or("モデルのパスが不正")?, context_params)
        .map_err(|e| format!("whisper のモデルを読み込めない: {e}"))?;
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;
    let _ = tx.send(Event::Asr(Stage::Ready));
    repaint();

    let context: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let mut seg = Segmenter::new();
    let mut last_text = String::new();
    let mut tick = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let (raw, leveled) = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if tick.elapsed() >= Duration::from_secs(2) {
            debug_log(&seg.debug());
            tick = std::time::Instant::now();
        }
        for utterance in seg.push(&raw, &leveled) {
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("auto"));
            params.set_translate(false);
            params.set_no_context(true);
            params.set_suppress_nst(true);
            params.set_no_speech_thold(0.6);
            params.set_temperature(0.0);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            let t0 = std::time::Instant::now();
            let ok = state.full(params, &utterance).is_ok();
            debug_log(&format!(
                "utterance {:.1}s decoded in {:.2}s ok={ok}",
                utterance.len() as f32 / SAMPLE_RATE as f32,
                t0.elapsed().as_secs_f32()
            ));
            if !ok {
                continue;
            }
            let mut text = String::new();
            for s in state.as_iter() {
                if s.no_speech_probability() > 0.6 {
                    continue;
                }
                text.push_str(&format!("{s}"));
            }
            let text = text.trim().to_string();
            if text.is_empty() || is_noise(&text) || (text == last_text && utterance.len() < 2 * SAMPLE_RATE as usize) {
                continue;
            }
            last_text.clone_from(&text);
            let lang = whisper_rs::get_lang_str(state.full_lang_id_from_state())
                .unwrap_or("??")
                .to_string();
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            debug_log(&format!("heard [{lang}] {text}"));
            let _ = tx.send(Event::Heard { id, lang: lang.clone(), text: text.clone() });
            repaint();

            let ctx_lines: Vec<String> = context.lock().map(|c| c.iter().cloned().collect()).unwrap_or_default();
            if let Ok(mut c) = context.lock() {
                c.push_back(text.clone());
                if c.len() > 3 {
                    c.pop_front();
                }
            }
            let (tx, settings, repaint) = (tx.clone(), settings.clone(), repaint.clone());
            std::thread::spawn(move || {
                let s = settings.lock().map(|s| s.clone()).unwrap_or_default();
                if s.engine == Engine::Off || lang == "ja" {
                    return;
                }
                let t0 = std::time::Instant::now();
                let ev = match translate::translate(&s, &text, &lang, &ctx_lines) {
                    Ok(text) => {
                        debug_log(&format!("translated in {:.2}s: {text}", t0.elapsed().as_secs_f32()));
                        Event::Translated { id, text }
                    }
                    Err(error) => Event::TranslateFailed { id, error },
                };
                let _ = tx.send(ev);
                repaint();
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod segmenter_tests {
    use super::*;

    /// One 20 ms frame of noise at the given RMS (a deterministic generator, so the test is repeatable).
    fn frame(rms: f32, seed: &mut u32) -> Vec<f32> {
        (0..FRAME)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / 8_388_608.0 - 1.0) * rms * 1.732
            })
            .collect()
    }

    /// Speech over a background that never goes quiet (wind, a crowd, music): two seconds of speech every
    /// three seconds, about three times as loud as the background, which itself swells and fades.
    #[test]
    fn speech_over_a_steady_background_is_still_split_into_utterances() {
        let mut seg = Segmenter::new();
        let mut seed = 7;
        let mut heard = Vec::new(); // start and end of each utterance, in frames
        let total = 50 * 90;
        for t in 0..total {
            let seconds = t as f32 / 50.0;
            let background = 0.006 + 0.003 * (seconds * 0.3 * std::f32::consts::TAU).sin();
            let speaking = seconds % 3.0 < 2.0;
            let syllable = 0.5 + 0.5 * (seconds * 4.0 * std::f32::consts::TAU).sin().abs();
            let level = if speaking { background + 0.02 * syllable } else { background };
            let chunk = frame(level, &mut seed);
            for utterance in seg.push(&chunk, &chunk) {
                heard.push((t as i64 - (utterance.len() / FRAME) as i64, t as i64));
            }
        }
        // A floor that drifts up to the level of the speech stops hearing it after some fifteen seconds, so
        // the stretches of speech after the first half minute are the ones that must still reach whisper.
        let missed = (10..29)
            .filter(|n| {
                let (from, to) = (n * 150, n * 150 + 100);
                !heard.iter().any(|(a, b)| *a < to - 25 && *b > from + 25)
            })
            .count();
        assert_eq!(missed, 0, "utterances: {heard:?}");
    }
}
