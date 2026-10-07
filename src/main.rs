#[allow(dead_code)]
mod app_shell_foundation;
mod agc;
mod band;
mod capture;
mod claude;
mod codex;
mod history;
mod pipeline;
mod sentences;
mod translate;

use app_shell_foundation::{
    app_shell_control_metrics, apply_app_shell_preferences, install_macos_system_fonts, load_app_shell_preferences,
    save_app_shell_preferences, show_app_shell_preferences, AppShellLanguage, AppShellPreferences,
    APP_SHELL_DARK_SELECTION, APP_SHELL_LIGHT_SELECTION, APP_SHELL_WEAK_TEXT,
};
use eframe::egui;
use pipeline::{Event, Pipeline, Stage};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use translate::{Engine, TranslateSettings};

const APP_SHELL_STORAGE_KEY: &str = "live-subtitle.app-shell-preferences.v1";
const SETTINGS_STORAGE_KEY: &str = "live-subtitle.settings.v1";
/// eframe's own storage key for the window position and size.
const EFRAME_WINDOW_STORAGE_KEY: &str = "window";
const MAX_LINES: usize = 5000;
const BAND_FIND_SECONDS: f32 = 10.0;
const BAND_FIND_YELLOW: egui::Color32 = egui::Color32::from_rgb(255, 214, 0);
/// Small, so the rounded frame still shows where the window's corners (resize handles) are.
const BAND_CORNER: f32 = 6.0;
/// Width of the strip along the band's edges that resizes it.
const BAND_EDGE: f32 = 8.0;
const NORMAL_MIN_SIZE: [f32; 2] = [380.0, 260.0];
const BAND_MIN_SIZE: [f32; 2] = [240.0, 70.0];
const METER_MIN_DB: f32 = -70.0;
const METER_SILENCE_DB: f32 = -60.0;
const METER_RELEASE_SECONDS: f32 = 0.12;
const METER_LOUD_DB: f32 = -25.0;
const METER_GREEN: egui::Color32 = egui::Color32::from_rgb(52, 199, 89);
const METER_YELLOW: egui::Color32 = egui::Color32::from_rgb(255, 204, 0);
const ERROR_RED: egui::Color32 = egui::Color32::from_rgb(220, 60, 60);

#[derive(Clone, Serialize, Deserialize)]
struct Persisted {
    translate: TranslateSettings,
    always_on_top: bool,
    show_original: bool,
    /// Height of the band. Its position is deliberately not remembered: it opens where the normal window
    /// was, because a band that reappears at an old position gets lost. Its width follows the normal window.
    #[serde(default)]
    band_height: Option<f32>,
    /// Folder chosen for saved conversations; the Desktop when unset.
    #[serde(default)]
    history_dir: Option<std::path::PathBuf>,
    /// How much the band's background lets the screen show through, in percent (0 is opaque).
    #[serde(default)]
    band_transparency: u8,
}

impl Default for Persisted {
    fn default() -> Self {
        Self { translate: TranslateSettings::default(), always_on_top: true, show_original: true, band_height: None, history_dir: None, band_transparency: 0 }
    }
}

/// Sign-in state of the account behind a translation engine (Claude or Codex).
#[derive(Clone, PartialEq)]
enum Auth {
    Unknown,
    Checking,
    SignedIn,
    SignedOut,
    SigningIn,
    Failed(String),
}

/// Why Ollama's memory is being freed; it prefixes the result shown in the window.
#[derive(Clone, Copy, PartialEq)]
enum Release {
    Startup,
    Manual,
    ModelSwitch,
}

impl Release {
    fn label(self, lang: AppShellLanguage) -> &'static str {
        match self {
            Release::Startup => tr(lang, "起動時", "At launch"),
            Release::Manual => tr(lang, "手動", "Manual"),
            Release::ModelSwitch => tr(lang, "モデル切り替え", "Model switch"),
        }
    }
}

enum Japanese {
    Pending,
    Done(String),
    Failed(String),
    /// Spoken in Japanese already, or translation switched off.
    NotNeeded,
}

struct Line {
    id: u64,
    at: chrono::DateTime<chrono::Local>,
    lang: String,
    original: String,
    japanese: Japanese,
}

/// Window state remembered while the band is shown, so ESC can restore the ordinary window.
struct BandState {
    restore_pos: egui::Pos2,
    restore_size: egui::Vec2,
    /// When the band opened; its frame blinks for `BAND_FIND_SECONDS` so it can be found.
    started: Instant,
    /// The edge drag in progress, if any.
    resizing: Option<BandResize>,
}

/// An edge drag of the band. macOS gives a borderless window only a hairline to grab, and winit cannot hand a
/// resize to AppKit there, so the band resizes itself from a wider strip along its edges.
struct BandResize {
    /// Which edges move: left, right, top, bottom.
    edges: [bool; 4],
    /// Pointer position in macOS screen points (y up) when the drag began.
    start_pointer: egui::Pos2,
    /// The window's rectangle (egui coordinates) when the drag began.
    start_rect: egui::Rect,
}

struct App {
    preferences: AppShellPreferences,
    /// The macOS language, used when the language preference is "System".
    system_language: AppShellLanguage,
    /// The language the window is shown in, resolved from the preference every frame.
    lang: AppShellLanguage,
    /// Whether the settings replace the subtitles in the window.
    settings_open: bool,
    persisted: Persisted,
    shared: pipeline::SharedSettings,
    pipeline: Option<Pipeline>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    lines: Vec<Line>,
    asr: Stage,
    translator: Stage,
    level: f32,
    gain_db: f32,
    error: Option<String>,
    ollama_models: Vec<String>,
    ollama_models_rx: Option<Receiver<Vec<String>>>,
    /// Text being typed into the Claude model field; committed on Enter or when focus leaves.
    claude_model_edit: String,
    codex_models: Vec<codex::ModelInfo>,
    codex_models_rx: Option<Receiver<Result<Vec<codex::ModelInfo>, String>>>,
    codex_models_tried: bool,
    claude_auth: Auth,
    codex_auth: Auth,
    auth_tx: Sender<(Engine, Auth)>,
    auth_rx: Receiver<(Engine, Auth)>,
    notice_tx: Sender<String>,
    notice_rx: Receiver<String>,
    band: Option<BandState>,
    band_configured: bool,
    /// Whether the level meter is on screen (not in the band); the pipeline reports levels only then.
    meter_shown: Arc<std::sync::atomic::AtomicBool>,
    auto_band: bool,
    /// One-line result of the last "save conversation" press.
    notice: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_macos_system_fonts(&cc.egui_ctx).expect("the managed macOS UI font must be available");
        let preferences = load_app_shell_preferences(cc.storage, APP_SHELL_STORAGE_KEY);
        apply_app_shell_preferences(&cc.egui_ctx, preferences);
        let mut persisted: Persisted = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_STORAGE_KEY))
            .unwrap_or_default();
        persisted.translate.claude_model = translate::explicit_claude_model(&persisted.translate.claude_model);
        cc.egui_ctx.send_viewport_cmd(level_command(persisted.always_on_top));
        let (tx, rx) = mpsc::channel();
        let (auth_tx, auth_rx) = mpsc::channel();
        let (notice_tx, notice_rx) = mpsc::channel();
        let autostart = std::env::var_os("LIVE_SUBTITLE_AUTOSTART").is_some();
        let system_language = system_language();
        let mut app = Self {
            preferences,
            system_language,
            lang: preferences.language.resolve(system_language),
            settings_open: false,
            shared: Arc::new(Mutex::new(persisted.translate.clone())),
            persisted,
            pipeline: None,
            tx,
            rx,
            lines: Vec::new(),
            asr: Stage::Idle,
            translator: Stage::Idle,
            level: 0.0,
            gain_db: 0.0,
            error: None,
            ollama_models: Vec::new(),
            ollama_models_rx: None,
            claude_model_edit: String::new(),
            codex_models: Vec::new(),
            codex_models_rx: None,
            codex_models_tried: false,
            claude_auth: Auth::Unknown,
            codex_auth: Auth::Unknown,
            auth_tx,
            auth_rx,
            notice_tx,
            notice_rx,
            band: None,
            // true on purpose: the first frame applies the ordinary (opaque) window traits.
            band_configured: true,
            meter_shown: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            notice: None,
            auto_band: std::env::var_os("LIVE_SUBTITLE_AUTOBAND").is_some(),
        };
        // A crash or a force-quit can leave models in memory; start from a clean slate.
        app.release_ollama(&cc.egui_ctx, Release::Startup, false);
        app.fetch_ollama_models(&cc.egui_ctx);
        if autostart {
            app.start(&cc.egui_ctx);
        }
        app
    }

    /// Writes the subtitles gathered so far to a new text file and shows it in Finder.
    fn save_conversation(&mut self) {
        let records: Vec<history::Record> = self
            .lines
            .iter()
            .map(|l| {
                let (japanese, note) = match &l.japanese {
                    Japanese::Done(ja) => (Some(ja.as_str()), None),
                    Japanese::Pending => (None, Some("翻訳中".to_string())),
                    Japanese::Failed(e) => (None, Some(format!("翻訳失敗: {e}"))),
                    Japanese::NotNeeded => (None, None),
                };
                history::Record { at: l.at, lang: &l.lang, original: &l.original, japanese, note }
            })
            .collect();
        self.notice = Some(match history::save(&history::history_dir(self.persisted.history_dir.as_deref()), &records) {
            Ok(path) => {
                let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
                match self.lang {
                    AppShellLanguage::Japanese => format!("{} 件を保存しました: {}", records.len(), path.display()),
                    AppShellLanguage::English => format!("Saved {} lines: {}", records.len(), path.display()),
                }
            }
            Err(e) => format!("{}: {e}", tr(self.lang, "保存できなかった", "Could not save")),
        });
    }

    /// Frees every model Ollama holds in memory, in the background, and shows the result in the window; with
    /// `warm_up_after` the translation model is loaded again once the memory is free.
    fn release_ollama(&mut self, ctx: &egui::Context, reason: Release, warm_up_after: bool) {
        let (notice, ctx, lang) = (self.notice_tx.clone(), ctx.clone(), self.lang);
        let warm = warm_up_after.then(|| (self.shared.clone(), self.tx.clone()));
        std::thread::spawn(move || {
            let label = reason.label(lang);
            let message = match (translate::ollama_unload_all(), lang) {
                (Ok(0), _) if reason == Release::Manual => {
                    tr(lang, "読み込み中の Ollama モデルはありません", "No Ollama model is loaded").to_string()
                }
                (Ok(0), _) => String::new(),
                (Ok(n), AppShellLanguage::Japanese) => format!("{label}: Ollama のモデルを {n} 個、メモリから解放した"),
                (Ok(n), AppShellLanguage::English) => format!("{label}: freed {n} Ollama model(s) from memory"),
                (Err(e), AppShellLanguage::Japanese) => format!("{label}: Ollama のモデルを解放できなかった: {e}"),
                (Err(e), AppShellLanguage::English) => format!("{label}: could not free Ollama's models: {e}"),
            };
            let _ = notice.send(message);
            if let Some((settings, tx)) = warm {
                let repaint_ctx = ctx.clone();
                pipeline::warm_up(settings, tx, move || repaint_ctx.request_repaint());
            }
            ctx.request_repaint();
        });
    }

    fn poll_notices(&mut self) {
        while let Ok(message) = self.notice_rx.try_recv() {
            self.notice = (!message.is_empty()).then_some(message);
        }
    }

    fn auth_mut(&mut self, engine: Engine) -> Option<&mut Auth> {
        match engine {
            Engine::Claude => Some(&mut self.claude_auth),
            Engine::Codex => Some(&mut self.codex_auth),
            Engine::Ollama | Engine::Off => None,
        }
    }

    fn poll_auth(&mut self) {
        while let Ok((engine, state)) = self.auth_rx.try_recv() {
            if let Some(slot) = self.auth_mut(engine) {
                *slot = state;
            }
        }
    }

    /// Reads the sign-in state in the background (read-only).
    fn check_auth(&mut self, engine: Engine, ctx: &egui::Context) {
        let Some(slot) = self.auth_mut(engine) else {
            return;
        };
        *slot = Auth::Checking;
        let (tx, ctx) = (self.auth_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = if engine == Engine::Claude { claude::signed_in() } else { codex::signed_in() };
            let state = match result {
                Ok(true) => Auth::SignedIn,
                Ok(false) => Auth::SignedOut,
                Err(e) => Auth::Failed(e),
            };
            let _ = tx.send((engine, state));
            ctx.request_repaint();
        });
    }

    /// Starts the official browser sign-in; only ever called from the sign-in button.
    fn sign_in(&mut self, engine: Engine, ctx: &egui::Context) {
        let Some(slot) = self.auth_mut(engine) else {
            return;
        };
        *slot = Auth::SigningIn;
        let (tx, ctx) = (self.auth_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = if engine == Engine::Claude { claude::sign_in() } else { codex::sign_in() };
            let state = match result {
                Ok(()) => Auth::SignedIn,
                Err(e) => Auth::Failed(e),
            };
            let _ = tx.send((engine, state));
            ctx.request_repaint();
        });
    }

    /// One line under the engine selector: whether the account is signed in, and the sign-in button.
    fn auth_row(&mut self, ui: &mut egui::Ui, engine: Engine) {
        let state = match engine {
            Engine::Claude => self.claude_auth.clone(),
            Engine::Codex => self.codex_auth.clone(),
            Engine::Ollama | Engine::Off => return,
        };
        let who = if engine == Engine::Claude { "Claude" } else { "ChatGPT" };
        let lang = self.lang;
        let say = |ja: &str, en: &str| match lang {
            AppShellLanguage::Japanese => format!("{who} {ja}"),
            AppShellLanguage::English => format!("{en} {who}"),
        };
        ui.horizontal_wrapped(|ui| match state {
            Auth::Unknown | Auth::Checking => {
                ui.spinner();
                ui.label(say("のサインインを確認中…", "Checking the sign-in to"));
            }
            Auth::SignedIn => {
                ui.colored_label(METER_GREEN, say("にサインイン済み", "Signed in to"));
            }
            Auth::SignedOut => {
                ui.colored_label(ERROR_RED, say("にサインインしていません", "Not signed in to"));
                if ui
                    .button(say("にサインイン", "Sign in to"))
                    .on_hover_text(tr(lang, "ブラウザで公式のサインイン画面を開く", "Opens the official sign-in page in the browser"))
                    .clicked()
                {
                    self.sign_in(engine, ui.ctx());
                }
            }
            Auth::SigningIn => {
                ui.spinner();
                ui.label(tr(lang, "ブラウザでサインインを完了してください…", "Finish signing in in the browser…"));
            }
            Auth::Failed(e) => {
                ui.colored_label(ERROR_RED, e);
                if ui.small_button(tr(lang, "再確認", "Check again")).clicked() {
                    self.check_auth(engine, ui.ctx());
                }
                if ui.small_button(say("にサインイン", "Sign in to")).clicked() {
                    self.sign_in(engine, ui.ctx());
                }
            }
        });
    }

    /// Asks Ollama for its model list in the background, so a server that does not answer cannot freeze the window.
    fn fetch_ollama_models(&mut self, ctx: &egui::Context) {
        if self.ollama_models_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.ollama_models_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(translate::ollama_models());
            ctx.request_repaint();
        });
    }

    fn poll_ollama_models(&mut self) {
        let Some(rx) = &self.ollama_models_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(models) => {
                self.ollama_models = models;
                self.ollama_models_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => self.ollama_models_rx = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn fetch_codex_models(&mut self, ctx: &egui::Context) {
        if self.codex_models_rx.is_some() {
            return;
        }
        self.codex_models_tried = true;
        let (tx, rx) = mpsc::channel();
        self.codex_models_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(codex::list_models());
            ctx.request_repaint();
        });
    }

    fn poll_codex_models(&mut self) {
        let Some(rx) = &self.codex_models_rx else {
            return;
        };
        if let Ok(result) = rx.try_recv() {
            self.codex_models_rx = None;
            match result {
                Ok(models) => self.codex_models = models,
                Err(e) => {
                    self.error = Some(format!("{}: {e}", tr(self.lang, "Codex のモデル一覧を取れない", "Could not list Codex's models")))
                }
            }
        }
    }

    fn running(&self) -> bool {
        self.pipeline.is_some()
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.error = None;
        let repaint = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        match Pipeline::start(self.shared.clone(), self.tx.clone(), repaint, self.meter_shown.clone()) {
            Ok(p) => self.pipeline = Some(p),
            Err(e) => self.error = Some(e),
        }
    }

    fn stop(&mut self) {
        self.pipeline = None;
        translate::shutdown();
        self.asr = Stage::Idle;
        self.translator = Stage::Idle;
        self.level = 0.0;
        self.gain_db = 0.0;
    }

    fn drain_events(&mut self, dt: f32) {
        let mut frame_peak = 0f32;
        while let Ok(ev) = self.rx.try_recv() {
            if !self.running() && !matches!(ev, Event::Translated { .. } | Event::TranslateFailed { .. }) {
                continue;
            }
            match ev {
                Event::Asr(s) => self.asr = s,
                Event::Translator(s) => self.translator = s,
                Event::Level { rms, gain_db } => {
                    frame_peak = frame_peak.max(rms);
                    self.gain_db = gain_db;
                }
                Event::Heard { id, lang, text } => {
                    let engine = self.persisted.translate.engine;
                    let japanese = if lang == "ja" || engine == Engine::Off {
                        Japanese::NotNeeded
                    } else {
                        Japanese::Pending
                    };
                    self.lines.push(Line { id, at: chrono::Local::now(), lang, original: text, japanese });
                    if self.lines.len() > MAX_LINES {
                        self.lines.remove(0);
                    }
                }
                Event::Revised { id, text } => {
                    if let Some(line) = self.lines.iter_mut().rev().find(|l| l.id == id) {
                        line.original = text;
                    }
                }
                Event::Translated { id, text } => {
                    self.set_japanese(id, Japanese::Done(text));
                }
                Event::TranslateFailed { id, error } => self.set_japanese(id, Japanese::Failed(error)),
                Event::Fatal(e) => {
                    self.error = Some(e);
                    self.stop();
                }
            }
        }
        // Rises instantly with the loudest chunk of this frame, falls with a short time constant.
        self.level = frame_peak.max(self.level * (-dt / METER_RELEASE_SECONDS).exp());
    }

    fn set_japanese(&mut self, id: u64, value: Japanese) {
        if let Some(line) = self.lines.iter_mut().rev().find(|l| l.id == id) {
            line.japanese = value;
        }
    }

    fn status_text(&self) -> String {
        let lang = self.lang;
        if !self.running() {
            return tr(lang, "停止中", "Stopped").into();
        }
        let mut parts = Vec::new();
        if self.asr == Stage::Loading {
            parts.push(tr(lang, "音声認識モデル読み込み中…", "Loading the speech model…"));
        }
        if self.translator == Stage::Loading {
            parts.push(tr(lang, "翻訳モデル読み込み中…", "Loading the translation model…"));
        }
        if parts.is_empty() {
            tr(lang, "聞き取り中", "Listening").into()
        } else {
            parts.join(" / ")
        }
    }

    fn loading(&self) -> bool {
        self.running() && (self.asr == Stage::Loading || self.translator == Stage::Loading)
    }

    fn copy_text(&self) -> String {
        self.lines
            .iter()
            .map(|l| match &l.japanese {
                Japanese::Done(ja) => format!("{ja}\n{}", l.original),
                _ => l.original.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The top of the window, always shown: start/stop and what the app is doing, the band and settings buttons,
    /// the level while running, the translation engine and model, and anything that needs the user (sign-in,
    /// errors, the result of the last action).
    fn controls(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        self.poll_codex_models();
        self.poll_ollama_models();
        self.poll_auth();
        self.poll_notices();
        let engine = self.persisted.translate.engine;
        if engine == Engine::Codex && !self.codex_models_tried {
            self.fetch_codex_models(ui.ctx());
        }
        if matches!(engine, Engine::Claude if self.claude_auth == Auth::Unknown)
            || matches!(engine, Engine::Codex if self.codex_auth == Auth::Unknown)
        {
            self.check_auth(engine, ui.ctx());
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let running = self.running();
            let (label, fill) = if running {
                (tr(lang, "■ 停止", "■ Stop"), ERROR_RED)
            } else if ui.visuals().dark_mode {
                (tr(lang, "● 開始", "● Start"), APP_SHELL_DARK_SELECTION)
            } else {
                (tr(lang, "● 開始", "● Start"), APP_SHELL_LIGHT_SELECTION)
            };
            let height = app_shell_control_metrics(ui.ctx()).row_height * 1.2;
            let button = egui::Button::new(egui::RichText::new(label).strong().color(egui::Color32::WHITE))
                .fill(fill)
                .min_size(egui::vec2(height * 2.8, height));
            if ui.add(button).clicked() {
                if running {
                    self.stop();
                } else {
                    self.start(ui.ctx());
                }
            }
            if self.loading() {
                ui.spinner();
            }
            if running {
                ui.label(self.status_text());
                if !self.loading() && self.level_db() < METER_SILENCE_DB {
                    ui.colored_label(APP_SHELL_WEAK_TEXT, tr(lang, "無音", "Silent"));
                }
            } else {
                ui.colored_label(APP_SHELL_WEAK_TEXT, self.status_text());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(tr(lang, "設定", "Settings")).selected(self.settings_open))
                    .on_hover_text(tr(
                        lang,
                        "翻訳・ウィンドウ・保存先・表示の設定を開く／閉じる",
                        "Open or close the translation, window, folder and appearance settings",
                    ))
                    .clicked()
                {
                    self.settings_open = !self.settings_open;
                }
                if ui
                    .button(tr(lang, "帯にする", "Caption bar"))
                    .on_hover_text(tr(
                        lang,
                        "字幕だけの軽量表示にする。ドラッグで移動、端でサイズ変更。ESC で元に戻る",
                        "Show only the subtitles on a slim bar. Drag to move, drag the edges to resize, Esc to return",
                    ))
                    .clicked()
                {
                    let _ = self.enter_band(ui.ctx());
                }
            });
        });
        if self.running() {
            self.level_bar(ui);
        }
        ui.horizontal_wrapped(|ui| {
            center_on_buttons(ui);
            ui.colored_label(APP_SHELL_WEAK_TEXT, tr(lang, "翻訳", "Translate"));
            self.translation_pickers(ui);
        });
        let signed_in = match engine {
            Engine::Claude => self.claude_auth == Auth::SignedIn,
            Engine::Codex => self.codex_auth == Auth::SignedIn,
            Engine::Ollama | Engine::Off => true,
        };
        // Open settings show the sign-in state in their translation section instead.
        if !signed_in && !self.settings_open {
            self.auth_row(ui, engine);
        }
        if let Some(e) = &self.error {
            ui.colored_label(ERROR_RED, e);
        }
        if let Some(notice) = &self.notice {
            ui.colored_label(APP_SHELL_WEAK_TEXT, notice);
        }
        ui.add_space(2.0);
    }

    fn level_db(&self) -> f32 {
        20.0 * self.level.max(1e-7).log10()
    }

    /// A thin bar across the window showing how loud the captured sound is.
    fn level_bar(&self, ui: &mut egui::Ui) {
        let db = self.level_db();
        let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 5.0), egui::Sense::hover());
        let fraction = ((db - METER_MIN_DB) / -METER_MIN_DB).clamp(0.0, 1.0);
        let color = if db < METER_SILENCE_DB {
            APP_SHELL_WEAK_TEXT
        } else if db > METER_LOUD_DB {
            METER_YELLOW
        } else {
            METER_GREEN
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 2.5, ui.visuals().extreme_bg_color);
        painter.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * fraction, rect.height())), 2.5, color);
        response.on_hover_text(match self.lang {
            AppShellLanguage::Japanese => format!("入力音声の大きさ（自動音量補正 +{:.0} dB）", self.gain_db),
            AppShellLanguage::English => format!("Level of the captured sound (automatic gain +{:.0} dB)", self.gain_db),
        });
    }

    /// The translation engine and its model (and Codex's effort). A change is handed to the pipeline by
    /// `apply_translate_change` at the end of the frame.
    fn translation_pickers(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        let t = &mut self.persisted.translate;
        egui::ComboBox::from_id_salt("engine").selected_text(t.engine.label(lang)).show_ui(ui, |ui| {
            for e in [Engine::Ollama, Engine::Claude, Engine::Codex, Engine::Off] {
                ui.selectable_value(&mut t.engine, e, e.label(lang));
            }
        });
        match t.engine {
            Engine::Ollama => {
                egui::ComboBox::from_id_salt("ollama-model").selected_text(t.ollama_model.clone()).show_ui(ui, |ui| {
                    let mut names = self.ollama_models.clone();
                    if !names.contains(&t.ollama_model) {
                        names.insert(0, t.ollama_model.clone());
                    }
                    for n in names {
                        ui.selectable_value(&mut t.ollama_model, n.clone(), n);
                    }
                });
            }
            Engine::Claude => {
                let label = translate::CLAUDE_MODELS
                    .iter()
                    .find(|(id, _, _)| *id == t.claude_model)
                    .map_or(t.claude_model.as_str(), |(_, ja, en)| tr(lang, ja, en));
                egui::ComboBox::from_id_salt("claude-model").selected_text(label).show_ui(ui, |ui| {
                    for (id, ja, en) in translate::CLAUDE_MODELS {
                        if ui.selectable_value(&mut t.claude_model, id.to_string(), tr(lang, ja, en)).changed() {
                            self.claude_model_edit = t.claude_model.clone();
                        }
                    }
                });
            }
            Engine::Codex => {
                let name = self
                    .codex_models
                    .iter()
                    .find(|m| m.id == t.codex_model)
                    .map_or(t.codex_model.as_str(), |m| m.name.as_str());
                egui::ComboBox::from_id_salt("codex-model").selected_text(name).show_ui(ui, |ui| {
                    for m in &self.codex_models {
                        if ui.selectable_value(&mut t.codex_model, m.id.clone(), &m.name).changed()
                            && !t.codex_effort.is_empty()
                            && !m.efforts.contains(&t.codex_effort)
                        {
                            t.codex_effort.clear();
                        }
                    }
                });
                let efforts = self
                    .codex_models
                    .iter()
                    .find(|m| m.id == t.codex_model)
                    .map(|m| m.efforts.clone())
                    .unwrap_or_default();
                let default = tr(lang, "既定", "default");
                let shown = if t.codex_effort.is_empty() { default } else { t.codex_effort.as_str() };
                let effort_label = format!("{}: {shown}", tr(lang, "考える強さ", "Effort"));
                egui::ComboBox::from_id_salt("codex-effort").selected_text(effort_label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut t.codex_effort, String::new(), default);
                    for e in efforts {
                        ui.selectable_value(&mut t.codex_effort, e.clone(), e);
                    }
                });
            }
            Engine::Off => {}
        }
    }

    /// Hands a translation setting changed this frame to the pipeline, and while running loads the newly chosen
    /// model.
    fn apply_translate_change(&mut self, ctx: &egui::Context, before: &TranslateSettings) {
        let t = &self.persisted.translate;
        let changed = t.engine != before.engine
            || t.ollama_model != before.ollama_model
            || t.claude_model != before.claude_model
            || t.codex_model != before.codex_model
            || t.codex_effort != before.codex_effort;
        if !changed {
            return;
        }
        if let Ok(mut s) = self.shared.lock() {
            *s = t.clone();
        }
        let reload = match t.engine {
            Engine::Ollama => before.engine != Engine::Ollama || before.ollama_model != t.ollama_model,
            Engine::Claude => before.engine != Engine::Claude || before.claude_model != t.claude_model,
            Engine::Codex => {
                before.engine != Engine::Codex || (&before.codex_model, &before.codex_effort) != (&t.codex_model, &t.codex_effort)
            }
            Engine::Off => false,
        };
        // Leaving an Ollama model (another model, or another engine) frees its memory first, so two large
        // models are never resident together; the new model is loaded once the memory is free.
        let left_ollama_model =
            before.engine == Engine::Ollama && (t.engine != Engine::Ollama || t.ollama_model != before.ollama_model);
        let warm_up_now = self.running() && reload;
        if left_ollama_model {
            self.release_ollama(ctx, Release::ModelSwitch, warm_up_now);
        } else if warm_up_now {
            let ctx = ctx.clone();
            pipeline::warm_up(self.shared.clone(), self.tx.clone(), move || ctx.request_repaint());
        }
    }

    /// What is changed rarely, in place of the subtitles while open.
    fn settings(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            settings_section(ui, tr(lang, "翻訳", "Translation"), |ui| match self.persisted.translate.engine {
                Engine::Ollama => {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(tr(lang, "モデル一覧を更新", "Refresh models")).clicked() {
                            self.fetch_ollama_models(ui.ctx());
                        }
                        if ui.button(tr(lang, "メモリ解放", "Free memory")).clicked() {
                            self.release_ollama(ui.ctx(), Release::Manual, false);
                        }
                    });
                    ui.colored_label(
                        APP_SHELL_WEAK_TEXT,
                        tr(
                            lang,
                            "メモリ解放: Ollama がメモリに載せている全モデルを解放する（他のアプリが使っているモデルも対象。次に使うとき再読み込みされる）",
                            "Free memory: unloads every model Ollama holds in memory (also those other apps use; they load again when next used)",
                        ),
                    );
                }
                Engine::Claude => {
                    self.auth_row(ui, Engine::Claude);
                    if self.claude_model_edit.is_empty() {
                        self.claude_model_edit = self.persisted.translate.claude_model.clone();
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(tr(lang, "モデル ID", "Model ID"));
                        let field = ui
                            .add(egui::TextEdit::singleline(&mut self.claude_model_edit).desired_width(220.0))
                            .on_hover_text(tr(
                                lang,
                                "一覧にないモデルを使うときに直接入力する（Enter で確定）",
                                "Type a model that is not in the list (Enter to apply)",
                            ));
                        let t = &mut self.persisted.translate;
                        if field.lost_focus() && self.claude_model_edit.trim() != t.claude_model {
                            let id = self.claude_model_edit.trim().to_string();
                            if !id.is_empty() {
                                t.claude_model = id;
                            }
                            self.claude_model_edit = t.claude_model.clone();
                        }
                    });
                }
                Engine::Codex => {
                    self.auth_row(ui, Engine::Codex);
                    if ui.button(tr(lang, "モデル一覧を更新", "Refresh models")).clicked() {
                        self.fetch_codex_models(ui.ctx());
                    }
                }
                Engine::Off => {
                    ui.colored_label(
                        APP_SHELL_WEAK_TEXT,
                        tr(lang, "翻訳しない設定です。翻訳先は上の欄で選べます。", "Translation is off. Choose an engine above."),
                    );
                }
            });
            settings_section(ui, tr(lang, "字幕", "Subtitles"), |ui| {
                ui.checkbox(&mut self.persisted.show_original, tr(lang, "原文も表示", "Show original"));
            });
            settings_section(ui, tr(lang, "帯", "Caption bar"), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(tr(lang, "背景の透過率", "Background transparency"));
                    ui.add(egui::Slider::new(&mut self.persisted.band_transparency, 0..=100).suffix(" %"));
                });
            });
            settings_section(ui, tr(lang, "ウィンドウ", "Window"), |ui| {
                if ui.checkbox(&mut self.persisted.always_on_top, tr(lang, "最前面に固定", "Keep on top")).changed() {
                    ui.ctx().send_viewport_cmd(level_command(self.persisted.always_on_top));
                }
            });
            settings_section(ui, tr(lang, "会話履歴の保存先", "History folder"), |ui| {
                let dir = history::history_dir(self.persisted.history_dir.as_deref());
                ui.label(history::display_dir(&dir));
                ui.horizontal_wrapped(|ui| {
                    if ui.button(tr(lang, "保存先を選ぶ…", "Choose folder…")).clicked() {
                        let title = tr(lang, "会話履歴の保存先", "History folder");
                        if let Some(dir) = rfd::FileDialog::new().set_title(title).set_directory(&dir).pick_folder() {
                            self.persisted.history_dir = Some(dir);
                        }
                    }
                    if self.persisted.history_dir.is_some() && ui.button(tr(lang, "デスクトップに戻す", "Use Desktop")).clicked() {
                        self.persisted.history_dir = None;
                    }
                });
            });
            settings_section(ui, tr(lang, "表示", "Appearance"), |ui| {
                let change = show_app_shell_preferences(ui, &mut self.preferences, self.system_language, "main-settings");
                if change.changed {
                    apply_app_shell_preferences(ui.ctx(), self.preferences);
                }
            });
            ui.add_space(4.0);
            if ui.button(tr(lang, "字幕に戻る", "Back to subtitles")).clicked() {
                self.settings_open = false;
            }
        });
    }

    /// The bar under the subtitles: what to show, and what to do with the subtitles gathered so far.
    fn actions(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            center_on_buttons(ui);
            ui.checkbox(&mut self.persisted.show_original, tr(lang, "原文も表示", "Show original"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let any = !self.lines.is_empty();
                if ui.add_enabled(any, egui::Button::new(tr(lang, "クリア", "Clear"))).clicked() {
                    self.lines.clear();
                }
                if ui
                    .add_enabled(any, egui::Button::new(tr(lang, "コピー", "Copy")))
                    .on_hover_text(tr(lang, "字幕をすべてクリップボードにコピーする", "Copy all subtitles to the clipboard"))
                    .clicked()
                {
                    ui.ctx().copy_text(self.copy_text());
                }
                if ui
                    .add_enabled(any, egui::Button::new(tr(lang, "履歴を保存", "Save")))
                    .on_hover_text(tr(
                        lang,
                        "いまの字幕（原文・訳・時刻）を、日時つきのテキストファイルに保存する（保存先は「設定」で選ぶ）",
                        "Save the subtitles so far (original, translation, time) to a dated text file (folder in Settings)",
                    ))
                    .clicked()
                {
                    self.save_conversation();
                }
            });
        });
        ui.add_space(2.0);
    }

    /// Switches to the compact subtitle view. Returns false when the window geometry is not known yet
    /// (the first frames after launch).
    fn enter_band(&mut self, ctx: &egui::Context) -> bool {
        let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
        let (Some(outer), Some(inner)) = (outer, inner) else {
            return false;
        };
        // The display is measured in macOS points; egui's window coordinates are those divided by the UI zoom.
        let zoom = ctx.zoom_factor();
        let display = band::display_rect_containing(outer.center() * zoom);
        let display = egui::Rect::from_min_max(display.min / zoom, display.max / zoom);
        let (main, original) = subtitle_sizes(&ctx.global_style());
        let height = band::band_height(main, original);
        // The band is as wide as the normal window was (resize that window to match the video), and as tall as
        // last time.
        // A remembered height is capped so the band can never come up as a huge slab.
        let size = egui::vec2(inner.width(), self.persisted.band_height.unwrap_or(height).min(display.height() * 0.25))
            .min(display.size() - egui::vec2(16.0, 16.0));
        let pos = band::centered_on(display, outer.center(), size);
        self.band = Some(BandState {
            restore_pos: outer.min,
            restore_size: inner.size(),
            started: Instant::now(),
            resizing: None,
        });
        self.meter_shown.store(false, std::sync::atomic::Ordering::Relaxed);
        use egui::ViewportCommand as Cmd;
        for cmd in [
            Cmd::Decorations(false),
            Cmd::Resizable(true),
            Cmd::MinInnerSize(egui::vec2(BAND_MIN_SIZE[0], BAND_MIN_SIZE[1])),
            Cmd::InnerSize(size),
            Cmd::OuterPosition(pos),
            level_command(true),
            Cmd::Focus,
        ] {
            ctx.send_viewport_cmd(cmd);
        }
        true
    }

    fn exit_band(&mut self, ctx: &egui::Context) {
        let Some(band) = self.band.take() else {
            return;
        };
        self.meter_shown.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            self.persisted.band_height = Some(rect.height());
        }
        use egui::ViewportCommand as Cmd;
        for cmd in [
            Cmd::Decorations(true),
            Cmd::Resizable(true),
            Cmd::MinInnerSize(egui::vec2(NORMAL_MIN_SIZE[0], NORMAL_MIN_SIZE[1])),
            Cmd::InnerSize(band.restore_size),
            Cmd::OuterPosition(band.restore_pos),
            level_command(self.persisted.always_on_top),
            Cmd::Focus,
        ] {
            ctx.send_viewport_cmd(cmd);
        }
    }

    /// The telop: the newest subtitle at the bottom of an always-shown strip with a hairline frame, its background
    /// as transparent as set in the settings. The text is the size of the window's subtitles; a taller strip shows
    /// earlier subtitles above the newest, dimmer.
    fn band_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Right after the band opens its frame blinks yellow, so it cannot be lost on a busy screen.
        let age = self.band.as_ref().map_or(f32::MAX, |b| b.started.elapsed().as_secs_f32());
        let finding = age < BAND_FIND_SECONDS;
        let pulse = if finding {
            ctx.request_repaint_after(Duration::from_millis(16));
            let fade_out = (BAND_FIND_SECONDS - age).clamp(0.0, 1.0);
            (0.5 + 0.5 * (age * std::f32::consts::TAU * 1.5).sin()) * fade_out
        } else {
            0.0
        };
        let (main_size, original_size) = subtitle_sizes(ui.style());
        let show_original = self.persisted.show_original;
        let mut exit = false;
        egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
            let rect = ui.max_rect();
            let hovered = ui.rect_contains_pointer(rect);
            let painter = ui.painter();
            let opacity = 255.0 * (1.0 - f32::from(self.persisted.band_transparency.min(100)) / 100.0);
            let fill = opacity.max(if finding { 90.0 } else { 0.0 });
            painter.rect_filled(rect, BAND_CORNER, egui::Color32::from_black_alpha(fill as u8));
            // A hairline frame, always, so the edges to drag for resizing can be found.
            let frame = egui::Color32::WHITE.gamma_multiply(if hovered { 0.75 } else { 0.4 });
            painter.rect_stroke(rect, BAND_CORNER, egui::Stroke::new(1.0, frame), egui::StrokeKind::Inside);
            if finding {
                let stroke = egui::Stroke::new(5.0, BAND_FIND_YELLOW.gamma_multiply(0.3 + 0.7 * pulse));
                painter.rect_stroke(rect.shrink(2.5), BAND_CORNER, stroke, egui::StrokeKind::Inside);
            }
            let drag = ui.interact(rect, egui::Id::new("band-drag"), egui::Sense::click_and_drag());
            let edges_at = |p: egui::Pos2| {
                [
                    p.x < rect.left() + BAND_EDGE,
                    p.x > rect.right() - BAND_EDGE,
                    p.y < rect.top() + BAND_EDGE,
                    p.y > rect.bottom() - BAND_EDGE,
                ]
            };
            let resizing = self.band.as_ref().and_then(|b| b.resizing.as_ref()).map(|r| r.edges);
            if let Some(icon) = resize_cursor(resizing.unwrap_or_else(|| drag.hover_pos().map_or([false; 4], edges_at))) {
                ctx.set_cursor_icon(icon);
            }
            if drag.drag_started() {
                // A drag starts only after the pointer has moved a little, so judge the edge where it was pressed.
                let (origin, now) = ctx.input(|i| (i.pointer.press_origin(), i.pointer.interact_pos()));
                let edges = origin.map_or([false; 4], edges_at);
                if edges.contains(&true) {
                    if let (Some(band), Some(start_rect), Some(origin), Some(now)) =
                        (&mut self.band, ctx.input(|i| i.viewport().outer_rect), origin, now)
                    {
                        // Where the pointer was pressed, in screen points (y up).
                        let moved = (now - origin) * ctx.zoom_factor();
                        let start_pointer = band::pointer_location() - egui::vec2(moved.x, -moved.y);
                        band.resizing = Some(BandResize { edges, start_pointer, start_rect });
                    }
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
            }
            if let Some(band) = &mut self.band {
                if drag.dragged() {
                    if let Some(r) = &band.resizing {
                        // Screen points are y-up; egui's window coordinates are y-down and divided by the UI zoom.
                        let moved = band::pointer_location() - r.start_pointer;
                        let delta = egui::vec2(moved.x, -moved.y) / ctx.zoom_factor();
                        let [left, right, top, bottom] = r.edges;
                        let mut new = r.start_rect;
                        let min = egui::vec2(BAND_MIN_SIZE[0], BAND_MIN_SIZE[1]);
                        if left {
                            new.min.x = (new.min.x + delta.x).min(new.max.x - min.x);
                        }
                        if right {
                            new.max.x = (new.max.x + delta.x).max(new.min.x + min.x);
                        }
                        if top {
                            new.min.y = (new.min.y + delta.y).min(new.max.y - min.y);
                        }
                        if bottom {
                            new.max.y = (new.max.y + delta.y).max(new.min.y + min.y);
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(new.min));
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(new.size()));
                    }
                }
                if !drag.dragged() {
                    band.resizing = None;
                }
            }
            // From the newest subtitle at the bottom upward, as many as fit.
            let width = rect.width() - 48.0;
            let left = rect.left() + 24.0;
            let top = rect.top() + 10.0;
            let mut bottom = rect.bottom() - 12.0;
            for (i, line) in self.lines.iter().rev().enumerate() {
                let dim = if i == 0 { 1.0 } else { 0.55 };
                let (main, color) = match &line.japanese {
                    Japanese::Done(ja) => (ja.as_str(), egui::Color32::WHITE),
                    Japanese::Pending => ("…", APP_SHELL_WEAK_TEXT),
                    Japanese::NotNeeded | Japanese::Failed(_) => (line.original.as_str(), egui::Color32::WHITE),
                };
                let main = painter.layout_job(band_job(main, main_size, color.gamma_multiply(dim), 2, width));
                let original = (show_original && matches!(line.japanese, Japanese::Done(_) | Japanese::Pending)).then(|| {
                    let text = format!("[{}] {}", line.lang, line.original);
                    painter.layout_job(band_job(&text, original_size, APP_SHELL_WEAK_TEXT.gamma_multiply(dim), 1, width))
                });
                let height = main.size().y + original.as_ref().map_or(0.0, |g| 4.0 + g.size().y);
                if i > 0 && bottom - height < top {
                    break;
                }
                let y = bottom - height;
                let original_y = y + main.size().y + 4.0;
                painter.galley(egui::pos2(left, y), main, egui::Color32::WHITE);
                if let Some(original) = original {
                    painter.galley(egui::pos2(left, original_y), original, egui::Color32::WHITE);
                }
                bottom = y - original_size * 0.6;
            }
            if finding && self.lines.is_empty() {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    tr(
                        self.lang,
                        "字幕を待っています　ドラッグで移動・端でサイズ変更・ESC で元に戻る",
                        "Waiting for subtitles · drag to move · drag the edges to resize · Esc to return",
                    ),
                    egui::FontId::proportional(original_size),
                    egui::Color32::WHITE.gamma_multiply(0.85),
                );
            }
            if hovered {
                let button = egui::Rect::from_min_size(rect.right_top() + egui::vec2(-150.0, 6.0), egui::vec2(144.0, 24.0));
                if ui.put(button, egui::Button::new(tr(self.lang, "元に戻す（ESC）", "Return (Esc)")).small()).clicked() {
                    exit = true;
                }
            }
        });
        if exit {
            self.exit_band(&ctx);
        }
    }

    fn subtitles(&self, ui: &mut egui::Ui) {
        let lang = self.lang;
        let (main_size, original_size) = subtitle_sizes(ui.style());
        if self.lines.is_empty() {
            let hint = if self.running() {
                tr(lang, "音声を待っています…", "Waiting for sound…")
            } else {
                tr(lang, "「開始」を押すと、Mac で鳴っている音を字幕にします。", "Press Start to subtitle the sound playing on this Mac.")
            };
            ui.centered_and_justified(|ui| ui.colored_label(APP_SHELL_WEAK_TEXT, hint));
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for line in &self.lines {
                let big = |t: &str| egui::RichText::new(t).size(main_size).strong();
                match &line.japanese {
                    Japanese::Done(ja) => {
                        ui.label(big(ja));
                    }
                    Japanese::Pending => {
                        ui.colored_label(APP_SHELL_WEAK_TEXT, tr(lang, "翻訳中…", "Translating…"));
                    }
                    Japanese::Failed(e) => {
                        ui.colored_label(ERROR_RED, format!("{}: {e}", tr(lang, "翻訳失敗", "Translation failed")));
                    }
                    Japanese::NotNeeded => {
                        ui.label(big(&line.original));
                    }
                }
                if self.persisted.show_original && !matches!(line.japanese, Japanese::NotNeeded) {
                    ui.colored_label(APP_SHELL_WEAK_TEXT, format!("[{}] {}", line.lang, line.original));
                }
                ui.add_space(original_size * 0.6);
            }
        });
    }
}

/// Text sizes of a subtitle (the translation, and the original under it), the same in the window and in the band.
/// Both follow the text-size setting through the UI zoom.
fn subtitle_sizes(style: &egui::Style) -> (f32, f32) {
    let body = egui::TextStyle::Body.resolve(style).size;
    (body * 1.3, body)
}

/// The resize cursor for the band edges `[left, right, top, bottom]` under the pointer.
fn resize_cursor([left, right, top, bottom]: [bool; 4]) -> Option<egui::CursorIcon> {
    use egui::CursorIcon as C;
    match (left || right, top || bottom) {
        (true, true) if (left && top) || (right && bottom) => Some(C::ResizeNwSe),
        (true, true) => Some(C::ResizeNeSw),
        (true, false) => Some(C::ResizeHorizontal),
        (false, true) => Some(C::ResizeVertical),
        (false, false) => None,
    }
}

fn tr(lang: AppShellLanguage, japanese: &'static str, english: &'static str) -> &'static str {
    match lang {
        AppShellLanguage::Japanese => japanese,
        AppShellLanguage::English => english,
    }
}

/// The first of the user's preferred macOS languages decides "System": Japanese when it is Japanese, else English.
fn system_language() -> AppShellLanguage {
    let preferred = objc2_foundation::NSLocale::preferredLanguages();
    match preferred.firstObject() {
        Some(first) if first.to_string().starts_with("ja") => AppShellLanguage::Japanese,
        _ => AppShellLanguage::English,
    }
}

/// Makes a row as tall as a button from the start, so labels, check boxes, buttons and drop-downs share one centre
/// line. Without it a drop-down (which sits in its own nested row that starts `interact_size.y` tall and grows
/// downward) ends up below the line, and a check box above it.
fn center_on_buttons(ui: &mut egui::Ui) {
    let height = ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y;
    ui.spacing_mut().interact_size.y = ui.spacing().interact_size.y.max(height);
    ui.set_row_height(ui.spacing().interact_size.y);
}

/// A titled group in the settings.
fn settings_section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(title).strong());
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

fn band_job(text: &str, size: f32, color: egui::Color32, rows: usize, width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: egui::FontId::proportional(size), color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: width,
        max_rows: rows,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job
}

fn level_command(on_top: bool) -> egui::ViewportCommand {
    egui::ViewportCommand::WindowLevel(if on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    })
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.lang = self.preferences.language.resolve(self.system_language);
        self.drain_events(ui.input(|i| i.unstable_dt).min(0.1));
        if self.band.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.exit_band(ui.ctx());
        }
        // Wait a few frames so the window has settled on its real position before it is remembered.
        if self.auto_band && ui.ctx().cumulative_frame_nr() > 30 && self.enter_band(ui.ctx()) {
            self.auto_band = false;
        }
        let band_active = self.band.is_some();
        if band_active || self.band_configured {
            band::configure_window(frame, band_active);
            self.band_configured = band_active;
        }
        if band_active {
            self.band_ui(ui);
        } else {
            let before = self.persisted.translate.clone();
            egui::Panel::top("controls").show_inside(ui, |ui| self.controls(ui));
            if self.settings_open {
                egui::CentralPanel::default().show_inside(ui, |ui| self.settings(ui));
            } else {
                egui::Panel::bottom("actions").show_inside(ui, |ui| self.actions(ui));
                egui::CentralPanel::default().show_inside(ui, |ui| self.subtitles(ui));
            }
            self.apply_translate_change(ui.ctx(), &before);
        }
    }

    fn on_exit(&mut self) {
        // Stop asking Ollama for anything, then free its memory before the process goes away.
        translate::close_ollama();
        self.pipeline = None;
        pipeline::finish_asr();
        translate::shutdown();
        let _ = translate::ollama_unload_all();
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.band.is_some() {
            [0.0; 4]
        } else {
            egui::Rgba::from(visuals.panel_fill).to_array()
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        save_app_shell_preferences(storage, APP_SHELL_STORAGE_KEY, &self.preferences);
        eframe::set_value(storage, SETTINGS_STORAGE_KEY, &self.persisted);
        // eframe restores the stored window geometry on every launch; blank it so the band's geometry
        // (or one saved by an earlier build) never becomes the normal window.
        storage.set_string(EFRAME_WINDOW_STORAGE_KEY, String::new());
    }
}

/// Frees Ollama's memory when the app ends abnormally: on a panic, and on SIGTERM/SIGINT/SIGHUP.
/// (A crash that gives no chance to run code is covered by the release at the next launch.)
/// Appends a line to `~/Library/Application Support/LiveSubtitle/crash.log`, so an unexpected end of the app
/// leaves a trace even when it was started from Finder and nobody saw its stderr.
fn log_crash_event(text: &str) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home).join("Library/Application Support/LiveSubtitle");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
        let _ = writeln!(file, "[{}] {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

fn install_safety_net() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_crash_event(&format!("panic: {info}\n{}", std::backtrace::Backtrace::force_capture()));
        let _ = translate::ollama_unload_all();
        previous(info);
    }));
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        std::thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                log_crash_event(&format!("received signal {signal}, shutting down"));
                translate::close_ollama();
                pipeline::finish_asr();
                translate::shutdown();
                let _ = translate::ollama_unload_all();
                std::process::exit(0);
            }
        });
    }
}

/// The Dock icon while the app runs. Without this eframe puts its own default icon there.
fn window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/window-icon-512.png"))
        .expect("assets/icon/window-icon-512.png must be a valid PNG")
}

fn main() -> eframe::Result {
    install_safety_net();
    let options = eframe::NativeOptions {
        // The band changes the window geometry; do not let that become the next launch's normal window.
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_icon(window_icon())
            .with_inner_size([680.0, 480.0])
            .with_min_inner_size(NORMAL_MIN_SIZE)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Live Subtitle", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
