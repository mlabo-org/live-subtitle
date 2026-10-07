//! capture → utterance segmentation → whisper → translation, each stage on its own thread.

use crate::agc::Agc;
use crate::capture::{Capture, SAMPLE_RATE};
use crate::sentences::{Assembler, Step};
use crate::translate::{self, Engine, TranslateSettings};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext, WhisperVadContextParams,
};

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
    /// The original text of a line already heard grew into a whole sentence (or was cut back to one).
    Revised { id: u64, text: String },
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

/// Silero VAD judges speech in windows of this many samples (32 ms).
const WINDOW: usize = 512;
const HOP: usize = 4; // windows judged per run of the detector (128 ms)
/// Earlier windows handed to the detector on every run. It starts each run from a blank state, and with less
/// than about two seconds behind the new windows it takes music for speech more often.
const CONTEXT: usize = 62;
const SPEECH_PROBABILITY: f32 = 0.5;
const PRE_ROLL: usize = 9; // about 300 ms kept before the first window with speech
const END_SILENCE: usize = 16; // about 500 ms without speech ends an utterance
const KEEP_TAIL: usize = 6; // windows without speech kept at the end of an utterance
const MIN_SPEECH: usize = 9; // shorter bursts are noise
const MAX_WINDOWS: usize = 312; // 10 s hard cap so continuous speech still produces subtitles
const CUT_WINDOW: usize = 94; // the cap cuts at the least speech-like window within the last 3 s

/// The detector's model (885 KB) travels inside the app. whisper.cpp loads it from a file, so it is written
/// next to the whisper model on first use.
const VAD_MODEL: &[u8] = include_bytes!("../assets/vad/ggml-silero-v5.1.2.bin");

fn vad_model_file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME が無い")?;
    let dir = home.join("Library/Application Support/LiveSubtitle");
    let file = dir.join("ggml-silero-v5.1.2.bin");
    if std::fs::metadata(&file).map(|m| m.len()).ok() != Some(VAD_MODEL.len() as u64) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作れない: {e}", dir.display()))?;
        let partial = dir.join(format!("ggml-silero-v5.1.2.bin.{}", std::process::id()));
        std::fs::write(&partial, VAD_MODEL)
            .and_then(|()| std::fs::rename(&partial, &file))
            .map_err(|e| format!("音声検出のモデルを書き出せない: {e}"))?;
    }
    Ok(file)
}

/// Utterance segmenter.
///
/// A voice activity detector (Silero VAD, run by whisper.cpp) says which windows hold speech, so wind, a
/// crowd or music are not taken for an utterance however loud they are. The windows are stitched into
/// utterances here. The detector hears the AGC-leveled audio, the same audio whisper gets: on the quiet
/// signal the Mac hands over it misses speech.
struct Segmenter {
    vad: WhisperVadContext,
    /// Audio not judged yet (less than `HOP` windows).
    carry: Vec<f32>,
    /// The last `CONTEXT` windows that were judged.
    context: Vec<f32>,
    frames: Vec<f32>,
    /// The detector's speech probability for each window in `frames`.
    probabilities: Vec<f32>,
    in_speech: bool,
    speech: usize,
    silence: usize,
}

impl Segmenter {
    fn new() -> Result<Self, String> {
        let model = vad_model_file()?;
        // One thread: the model is tiny, and with the default four the workers spend more CPU time waiting on
        // each other than the single thread needs for the whole job.
        let mut params = WhisperVadContextParams::default();
        params.set_n_threads(1);
        let vad = WhisperVadContext::new(model.to_str().ok_or("モデルのパスが不正")?, params)
            .map_err(|e| format!("音声検出のモデルを読み込めない: {e}"))?;
        Ok(Self {
            vad,
            carry: Vec::new(),
            context: Vec::new(),
            frames: Vec::new(),
            probabilities: Vec::new(),
            in_speech: false,
            speech: 0,
            silence: 0,
        })
    }

    fn debug(&self) -> String {
        format!(
            "in_speech={} speech={} silence={} windows={} speech_probability={:.2}",
            self.in_speech,
            self.speech,
            self.silence,
            self.probabilities.len(),
            self.probabilities.last().copied().unwrap_or(0.0)
        )
    }

    /// Whether an utterance is being collected right now.
    fn speaking(&self) -> bool {
        self.in_speech
    }

    fn reset(&mut self) {
        self.frames.clear();
        self.probabilities.clear();
        self.in_speech = false;
        self.speech = 0;
        self.silence = 0;
    }

    /// Takes leveled audio as it arrives and returns the utterances that ended in it.
    fn push(&mut self, leveled: &[f32]) -> Result<Vec<Vec<f32>>, String> {
        self.carry.extend_from_slice(leveled);
        let mut out = Vec::new();
        while self.carry.len() >= HOP * WINDOW {
            let fresh: Vec<f32> = self.carry.drain(..HOP * WINDOW).collect();
            let behind = self.context.len() / WINDOW;
            self.context.extend_from_slice(&fresh);
            self.vad.detect_speech(&self.context).map_err(|e| format!("音声検出に失敗: {e}"))?;
            let judged: Vec<f32> =
                self.vad.probabilities().get(behind..behind + HOP).ok_or("音声検出の結果が足りない")?.to_vec();
            let excess = self.context.len().saturating_sub(CONTEXT * WINDOW);
            self.context.drain(..excess);
            for (window, probability) in fresh.chunks_exact(WINDOW).zip(judged) {
                self.step(window, probability, &mut out);
            }
        }
        Ok(out)
    }

    fn step(&mut self, window: &[f32], probability: f32, out: &mut Vec<Vec<f32>>) {
        self.frames.extend_from_slice(window);
        self.probabilities.push(probability);
        if probability > SPEECH_PROBABILITY {
            self.speech += 1;
            self.silence = 0;
            self.in_speech = true;
        } else if self.in_speech {
            self.silence += 1;
        }
        if !self.in_speech {
            if self.probabilities.len() > PRE_ROLL {
                self.frames.drain(..WINDOW);
                self.probabilities.remove(0);
            }
            return;
        }
        if self.silence >= END_SILENCE {
            let end = self.frames.len() - self.silence.saturating_sub(KEEP_TAIL) * WINDOW;
            let enough = self.speech >= MIN_SPEECH;
            let utterance = self.frames[..end].to_vec();
            self.reset();
            if enough {
                out.push(utterance);
            }
        } else if self.probabilities.len() >= MAX_WINDOWS {
            let from = self.probabilities.len() - CUT_WINDOW;
            let cut = from
                + self.probabilities[from..]
                    .iter()
                    .enumerate()
                    .min_by(|a, b| a.1.total_cmp(b.1))
                    .map_or(0, |(i, _)| i);
            out.push(self.frames[..cut * WINDOW].to_vec());
            self.frames.drain(..cut * WINDOW);
            self.probabilities.drain(..cut);
            self.speech = self.probabilities.len();
            self.silence = 0;
        }
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
    out: Sender<Vec<f32>>,
    tx: &Sender<Event>,
    repaint: &(impl Fn() + Send + Sync),
    stop: &AtomicBool,
    meter: &AtomicBool,
) {
    let mut agc = Agc::new();
    let (mut chunks, mut peak, mut tick) = (0usize, 0f32, std::time::Instant::now());
    let (mut meter_peak, mut meter_tick) = (0f32, std::time::Instant::now());
    while !stop.load(Ordering::Relaxed) {
        let mut leveled = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
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
        if out.send(leveled).is_err() {
            break;
        }
    }
}

fn run_asr(
    rx: Receiver<Vec<f32>>,
    model: &PathBuf,
    settings: SharedSettings,
    tx: &Sender<Event>,
    repaint: &(impl Fn() + Send + Sync + Clone + 'static),
    stop: &AtomicBool,
) -> Result<(), String> {
    // whisper-rs switches flash attention off by default; whisper.cpp itself has it on, and it is faster on Metal.
    let mut context_params = WhisperContextParameters::default();
    context_params.flash_attn(true);
    // whisper.cpp reports every detector run and every decode on stderr; nothing reads it.
    whisper_rs::install_logging_hooks();
    let ctx = WhisperContext::new_with_params(model.to_str().ok_or("モデルのパスが不正")?, context_params)
        .map_err(|e| format!("whisper のモデルを読み込めない: {e}"))?;
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;
    let _ = tx.send(Event::Asr(Stage::Ready));
    repaint();

    let context: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let mut seg = Segmenter::new()?;
    let mut sentences = Assembler::default();
    let mut last_text = String::new();
    let mut tick = std::time::Instant::now();
    let send = |step| send_step(step, &context, &settings, tx, repaint);

    while !stop.load(Ordering::Relaxed) {
        if sentences.due(std::time::Instant::now(), seg.speaking()) {
            sentences.flush().into_iter().for_each(send);
        }
        let leveled = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if tick.elapsed() >= Duration::from_secs(2) {
            debug_log(&seg.debug());
            tick = std::time::Instant::now();
        }
        for utterance in seg.push(&leveled)? {
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
            debug_log(&format!("heard [{lang}] {text}"));
            let off = settings.lock().is_ok_and(|s| s.engine == Engine::Off);
            if lang == "ja" || off {
                // Nothing to translate, so nothing is held back: the line shows as it was heard.
                sentences.flush().into_iter().for_each(send);
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                send(Step::Show { id, lang, text });
            } else {
                let next_id = || NEXT_ID.fetch_add(1, Ordering::Relaxed);
                sentences.hear(&lang, &text, next_id, std::time::Instant::now()).into_iter().for_each(send);
            }
        }
    }
    // A sentence still waiting when listening stops is translated as it is; its translation may land after stop.
    sentences.flush().into_iter().for_each(send);
    Ok(())
}


/// Carries out one step of the sentence assembler: shows or revises a line, or translates it on its own thread.
fn send_step(
    step: Step,
    context: &Arc<Mutex<VecDeque<String>>>,
    settings: &SharedSettings,
    tx: &Sender<Event>,
    repaint: &(impl Fn() + Send + Sync + Clone + 'static),
) {
    let (id, lang, text) = match step {
        Step::Show { id, lang, text } => {
            let _ = tx.send(Event::Heard { id, lang, text });
            repaint();
            return;
        }
        Step::Revise { id, text } => {
            let _ = tx.send(Event::Revised { id, text });
            repaint();
            return;
        }
        Step::Translate { id, lang, text } => (id, lang, text),
    };
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

#[cfg(test)]
mod segmenter_tests {
    use super::*;

    /// Twenty seconds of sound without a voice must not reach whisper, however loud it is: gusty wind
    /// (low-passed noise that swells), and plain noise.
    #[test]
    fn loud_sound_without_a_voice_is_not_an_utterance() {
        for gusty in [true, false] {
            let mut seg = Segmenter::new().unwrap();
            let (mut seed, mut low) = (7u32, 0f32);
            let mut utterances = 0;
            for t in 0..(SAMPLE_RATE as usize * 20 / 320) {
                let chunk: Vec<f32> = (0..320)
                    .map(|i| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        let white = (seed >> 8) as f32 / 8_388_608.0 - 1.0;
                        if !gusty {
                            return white * 0.17;
                        }
                        low = low * 0.97 + white * 0.03;
                        let seconds = (t * 320 + i) as f32 / SAMPLE_RATE as f32;
                        low * if seconds % 4.0 < 1.5 { 4.0 } else { 0.8 }
                    })
                    .collect();
                utterances += seg.push(&chunk).unwrap().len();
            }
            assert_eq!(utterances, 0, "gusty={gusty}");
        }
    }
}
