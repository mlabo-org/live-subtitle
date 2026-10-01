#[allow(dead_code)]
mod app_shell_foundation;
mod agc;
mod band;
mod capture;
mod claude;
mod codex;
mod history;
mod pipeline;
mod translate;

use app_shell_foundation::{
    apply_app_shell_preferences, install_macos_system_fonts, load_app_shell_preferences,
    save_app_shell_preferences, show_app_shell_preferences, AppShellLanguage, AppShellPreferences,
    APP_SHELL_WEAK_TEXT,
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
const BAND_VISIBLE_SECONDS: f64 = 10.0;
const NORMAL_MIN_SIZE: [f32; 2] = [380.0, 260.0];
const BAND_MIN_SIZE: [f32; 2] = [240.0, 70.0];
const METER_MIN_DB: f32 = -70.0;
const METER_SILENCE_DB: f32 = -60.0;
const METER_RELEASE_SECONDS: f32 = 0.12;
const METER_LOUD_DB: f32 = -25.0;
const METER_GREEN: egui::Color32 = egui::Color32::from_rgb(52, 199, 89);
const METER_YELLOW: egui::Color32 = egui::Color32::from_rgb(255, 204, 0);

#[derive(Clone, Serialize, Deserialize)]
struct Persisted {
    translate: TranslateSettings,
    always_on_top: bool,
    show_original: bool,
    /// Last position and size of the band (x, y, width, height), restored the next time it opens.
    #[serde(default)]
    band_rect: Option<[f32; 4]>,
    /// Folder chosen for saved conversations; the Desktop when unset.
    #[serde(default)]
    history_dir: Option<std::path::PathBuf>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self { translate: TranslateSettings::default(), always_on_top: true, show_original: true, band_rect: None, history_dir: None }
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
    passthrough: bool,
}

struct App {
    preferences: AppShellPreferences,
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
    last_line_at: Option<Instant>,
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
        let mut app = Self {
            preferences,
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
            ollama_models: translate::ollama_models(),
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
            band_configured: false,
            last_line_at: None,
            notice: None,
            auto_band: std::env::var_os("LIVE_SUBTITLE_AUTOBAND").is_some(),
        };
        // A crash or a force-quit can leave models in memory; start from a clean slate.
        app.release_ollama(&cc.egui_ctx, "起動時", false);
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
                format!("{} 件を保存しました: {}", records.len(), path.display())
            }
            Err(e) => format!("保存できなかった: {e}"),
        });
    }

    /// Frees every model Ollama holds in memory, in the background. `reason` prefixes the result shown in the
    /// window; with `warm_up_after` the translation model is loaded again once the memory is free.
    fn release_ollama(&mut self, ctx: &egui::Context, reason: &'static str, warm_up_after: bool) {
        let (notice, ctx) = (self.notice_tx.clone(), ctx.clone());
        let warm = warm_up_after.then(|| (self.shared.clone(), self.tx.clone()));
        std::thread::spawn(move || {
            let message = match translate::ollama_unload_all() {
                Ok(0) if reason == "手動" => "読み込み中の Ollama モデルはありません".to_string(),
                Ok(0) => String::new(),
                Ok(n) => format!("{reason}: Ollama のモデルを {n} 個、メモリから解放した"),
                Err(e) => format!("{reason}: Ollama のモデルを解放できなかった: {e}"),
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
        let red = egui::Color32::from_rgb(220, 60, 60);
        ui.horizontal_wrapped(|ui| match state {
            Auth::Unknown | Auth::Checking => {
                ui.spinner();
                ui.label(format!("{who} のサインインを確認中…"));
            }
            Auth::SignedIn => {
                ui.colored_label(METER_GREEN, format!("{who} にサインイン済み"));
            }
            Auth::SignedOut => {
                ui.colored_label(red, format!("{who} にサインインしていません"));
                if ui.button(format!("{who} にサインイン")).on_hover_text("ブラウザで公式のサインイン画面を開く").clicked() {
                    self.sign_in(engine, ui.ctx());
                }
            }
            Auth::SigningIn => {
                ui.spinner();
                ui.label("ブラウザでサインインを完了してください…");
            }
            Auth::Failed(e) => {
                ui.colored_label(red, e);
                if ui.small_button("再確認").clicked() {
                    self.check_auth(engine, ui.ctx());
                }
                if ui.small_button(format!("{who} にサインイン")).clicked() {
                    self.sign_in(engine, ui.ctx());
                }
            }
        });
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
                Err(e) => self.error = Some(format!("Codex のモデル一覧を取れない: {e}")),
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
        match Pipeline::start(self.shared.clone(), self.tx.clone(), repaint) {
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
                    self.last_line_at = Some(Instant::now());
                    if self.lines.len() > MAX_LINES {
                        self.lines.remove(0);
                    }
                }
                Event::Translated { id, text } => {
                    self.set_japanese(id, Japanese::Done(text));
                    self.last_line_at = Some(Instant::now());
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
        if !self.running() {
            return "停止中".into();
        }
        let mut parts = Vec::new();
        if self.asr == Stage::Loading {
            parts.push("音声認識モデル読み込み中…");
        }
        if self.translator == Stage::Loading {
            parts.push("翻訳モデル読み込み中…");
        }
        if parts.is_empty() {
            "聞き取り中".into()
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

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let label = if self.running() { "■ 停止" } else { "● 開始" };
            if ui.button(label).clicked() {
                if self.running() {
                    self.stop();
                } else {
                    self.start(ui.ctx());
                }
            }
            if self.loading() {
                ui.spinner();
            }
            ui.label(self.status_text());
        });
        if self.running() {
            ui.horizontal(|ui| {
                let db = 20.0 * self.level.max(1e-7).log10();
                let silent = db < METER_SILENCE_DB;
                ui.label("入力音声");
                let mut bar = egui::ProgressBar::new(((db - METER_MIN_DB) / -METER_MIN_DB).clamp(0.0, 1.0))
                    .desired_width(160.0)
                    .fill(if silent {
                        APP_SHELL_WEAK_TEXT
                    } else if db > METER_LOUD_DB {
                        METER_YELLOW
                    } else {
                        METER_GREEN
                    });
                if silent {
                    bar = bar.text("無音");
                }
                ui.add(bar).on_hover_text(format!("自動音量補正（AGC）: +{:.0} dB", self.gain_db));
            });
        }
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), e);
        }
        ui.horizontal_wrapped(|ui| {
            let dir = history::history_dir(self.persisted.history_dir.as_deref());
            ui.colored_label(APP_SHELL_WEAK_TEXT, format!("履歴の保存先: {}", history::display_dir(&dir)));
            if self.persisted.history_dir.is_some() && ui.small_button("デスクトップに戻す").clicked() {
                self.persisted.history_dir = None;
            }
        });
        if let Some(notice) = &self.notice {
            ui.colored_label(APP_SHELL_WEAK_TEXT, notice);
        }
        ui.horizontal_wrapped(|ui| {
            let mut changed = false;
            let before_engine = self.persisted.translate.engine;
            let before_model = self.persisted.translate.ollama_model.clone();
            let before_claude = self.persisted.translate.claude_model.clone();
            let before_codex = (self.persisted.translate.codex_model.clone(), self.persisted.translate.codex_effort.clone());
            self.poll_codex_models();
            if self.persisted.translate.engine == Engine::Codex && !self.codex_models_tried {
                self.fetch_codex_models(ui.ctx());
            }
            ui.label("翻訳先");
            egui::ComboBox::from_id_salt("engine")
                .selected_text(self.persisted.translate.engine.label())
                .show_ui(ui, |ui| {
                    for e in [Engine::Ollama, Engine::Claude, Engine::Codex, Engine::Off] {
                        changed |= ui
                            .selectable_value(&mut self.persisted.translate.engine, e, e.label())
                            .changed();
                    }
                });
            match self.persisted.translate.engine {
                Engine::Ollama => {
                    let t = &mut self.persisted.translate;
                    egui::ComboBox::from_id_salt("ollama-model")
                        .selected_text(t.ollama_model.clone())
                        .show_ui(ui, |ui| {
                            let mut names = self.ollama_models.clone();
                            if !names.contains(&t.ollama_model) {
                                names.insert(0, t.ollama_model.clone());
                            }
                            for n in names {
                                changed |= ui.selectable_value(&mut t.ollama_model, n.clone(), n).changed();
                            }
                        });
                    if ui.small_button("更新").on_hover_text("Ollama のモデル一覧を取り直す").clicked() {
                        self.ollama_models = translate::ollama_models();
                    }
                    if ui
                        .button("メモリ解放")
                        .on_hover_text("Ollama がメモリに載せている全モデルを解放する（他のアプリが使っているモデルも対象。次に使うとき再読み込みされる）")
                        .clicked()
                    {
                        self.release_ollama(ui.ctx(), "手動", false);
                    }
                }
                Engine::Claude => {
                    let t = &mut self.persisted.translate;
                    let label = translate::CLAUDE_MODELS
                        .iter()
                        .find(|(id, _)| *id == t.claude_model)
                        .map_or(t.claude_model.as_str(), |(_, label)| *label);
                    egui::ComboBox::from_id_salt("claude-model").selected_text(label).show_ui(ui, |ui| {
                        for (id, label) in translate::CLAUDE_MODELS {
                            if ui.selectable_value(&mut t.claude_model, id.to_string(), label).changed() {
                                changed = true;
                                self.claude_model_edit = t.claude_model.clone();
                            }
                        }
                    });
                    if self.claude_model_edit.is_empty() {
                        self.claude_model_edit = t.claude_model.clone();
                    }
                    let field = ui
                        .add(egui::TextEdit::singleline(&mut self.claude_model_edit).desired_width(210.0))
                        .on_hover_text("モデル ID を直接入力できる（Enter で確定）");
                    if field.lost_focus() && self.claude_model_edit.trim() != t.claude_model {
                        let id = self.claude_model_edit.trim().to_string();
                        if !id.is_empty() {
                            t.claude_model = id;
                            changed = true;
                        }
                        self.claude_model_edit = t.claude_model.clone();
                    }
                }
                Engine::Codex => {
                    let t = &mut self.persisted.translate;
                    let name = self
                        .codex_models
                        .iter()
                        .find(|m| m.id == t.codex_model)
                        .map_or(t.codex_model.as_str(), |m| m.name.as_str());
                    egui::ComboBox::from_id_salt("codex-model").selected_text(name).show_ui(ui, |ui| {
                        for m in &self.codex_models {
                            if ui.selectable_value(&mut t.codex_model, m.id.clone(), &m.name).changed() {
                                changed = true;
                                if !t.codex_effort.is_empty() && !m.efforts.contains(&t.codex_effort) {
                                    t.codex_effort.clear();
                                }
                            }
                        }
                    });
                    let efforts = self
                        .codex_models
                        .iter()
                        .find(|m| m.id == t.codex_model)
                        .map(|m| m.efforts.clone())
                        .unwrap_or_default();
                    let effort_label = if t.codex_effort.is_empty() {
                        "考える強さ: 既定".to_string()
                    } else {
                        format!("考える強さ: {}", t.codex_effort)
                    };
                    egui::ComboBox::from_id_salt("codex-effort").selected_text(effort_label).show_ui(ui, |ui| {
                        changed |= ui.selectable_value(&mut t.codex_effort, String::new(), "既定").changed();
                        for e in efforts {
                            changed |= ui.selectable_value(&mut t.codex_effort, e.clone(), e).changed();
                        }
                    });
                    if ui.small_button("更新").on_hover_text("Codex で使えるモデルの一覧を取り直す").clicked() {
                        self.fetch_codex_models(ui.ctx());
                    }
                }
                Engine::Off => {}
            }
            if changed {
                if let Ok(mut s) = self.shared.lock() {
                    *s = self.persisted.translate.clone();
                }
                let t = &self.persisted.translate;
                let reload = match t.engine {
                    Engine::Ollama => before_engine != Engine::Ollama || before_model != t.ollama_model,
                    Engine::Claude => before_engine != Engine::Claude || before_claude != t.claude_model,
                    Engine::Codex => {
                        before_engine != Engine::Codex || before_codex != (t.codex_model.clone(), t.codex_effort.clone())
                    }
                    Engine::Off => false,
                };
                // Leaving an Ollama model (another model, or another engine) frees its memory first, so two large
                // models are never resident together; the new model is loaded once the memory is free.
                let left_ollama_model = before_engine == Engine::Ollama
                    && (self.persisted.translate.engine != Engine::Ollama
                        || self.persisted.translate.ollama_model != before_model);
                let warm_up_now = self.running() && reload;
                if left_ollama_model {
                    self.release_ollama(ui.ctx(), "モデル切り替え", warm_up_now);
                } else if warm_up_now {
                    let ctx = ui.ctx().clone();
                    pipeline::warm_up(self.shared.clone(), self.tx.clone(), move || ctx.request_repaint());
                }
            }
        });
        self.poll_auth();
        self.poll_notices();
        let engine = self.persisted.translate.engine;
        if matches!(engine, Engine::Claude | Engine::Codex) {
            let unknown = matches!(engine, Engine::Claude if self.claude_auth == Auth::Unknown)
                || matches!(engine, Engine::Codex if self.codex_auth == Auth::Unknown);
            if unknown {
                self.check_auth(engine, ui.ctx());
            }
            self.auth_row(ui, engine);
        }
        ui.horizontal_wrapped(|ui| {
            if ui.checkbox(&mut self.persisted.always_on_top, "最前面に固定").changed() {
                ui.ctx().send_viewport_cmd(level_command(self.persisted.always_on_top));
            }
            ui.checkbox(&mut self.persisted.show_original, "原文も表示");
            if ui
                .add_enabled(!self.lines.is_empty(), egui::Button::new("会話履歴を保存"))
                .on_hover_text("いまの字幕（原文・訳・時刻）を、日時つきのテキストファイルに保存する")
                .clicked()
            {
                self.save_conversation();
            }
            if ui.button("保存先を選ぶ…").on_hover_text("会話履歴を保存するフォルダを選ぶ（既定はデスクトップ）").clicked() {
                let start = history::history_dir(self.persisted.history_dir.as_deref());
                if let Some(dir) = rfd::FileDialog::new().set_title("会話履歴の保存先").set_directory(start).pick_folder() {
                    self.persisted.history_dir = Some(dir);
                }
            }
            if ui
                .button("帯にする")
                .on_hover_text("字幕だけの軽量表示にする。ドラッグで移動、端でサイズ変更。ESC で元に戻る")
                .clicked()
            {
                let _ = self.enter_band(ui.ctx());
            }
            if ui.button("クリア").clicked() {
                self.lines.clear();
            }
            if ui.button("コピー").clicked() {
                ui.ctx().copy_text(self.copy_text());
            }
        });
        egui::CollapsingHeader::new("表示設定").show(ui, |ui| {
            let change = show_app_shell_preferences(ui, &mut self.preferences, AppShellLanguage::Japanese, "main-settings");
            if change.changed {
                apply_app_shell_preferences(ui.ctx(), self.preferences);
            }
        });
    }

    /// Switches to the compact subtitle view. Returns false when the window geometry is not known yet
    /// (the first frames after launch).
    fn enter_band(&mut self, ctx: &egui::Context) -> bool {
        let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
        let (Some(outer), Some(inner)) = (outer, inner) else {
            return false;
        };
        let (pos, size) = match self.persisted.band_rect {
            Some([x, y, w, h]) => (egui::pos2(x, y), egui::vec2(w, h)),
            None => {
                // The display is measured in macOS points; egui's window coordinates are those divided by the UI zoom.
                let zoom = ctx.zoom_factor();
                let display = band::display_rect_containing(outer.center() * zoom);
                let display = egui::Rect::from_min_max(display.min / zoom, display.max / zoom);
                band::default_band(display, band::band_height(f32::from(self.preferences.font_size_points)))
            }
        };
        self.band = Some(BandState { restore_pos: outer.min, restore_size: inner.size(), passthrough: false });
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
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            self.persisted.band_rect = Some([rect.min.x, rect.min.y, rect.width(), rect.height()]);
        }
        use egui::ViewportCommand as Cmd;
        for cmd in [
            Cmd::MousePassthrough(false),
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

    /// The telop: the newest subtitle on a translucent strip that fades out when nothing is being said.
    fn band_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let latest = self.lines.last();
        let pending = matches!(latest.map(|l| &l.japanese), Some(Japanese::Pending));
        let fresh = self.last_line_at.is_some_and(|t| t.elapsed().as_secs_f64() < BAND_VISIBLE_SECONDS);
        let visible = latest.is_some() && (pending || fresh);
        if latest.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        let alpha = ctx.animate_bool_with_time(egui::Id::new("band-visible"), visible, 0.25);
        // While nothing is shown the strip lets mouse clicks through to the video underneath.
        if let Some(band) = &mut self.band {
            if band.passthrough == visible {
                band.passthrough = !visible;
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(!visible));
            }
        }
        let size = band::unit_for_height(ctx.content_rect().height());
        let content = latest.map(|l| {
            let (main, color) = match &l.japanese {
                Japanese::Done(ja) => (ja.clone(), egui::Color32::WHITE),
                Japanese::Pending => ("…".to_string(), APP_SHELL_WEAK_TEXT),
                Japanese::NotNeeded | Japanese::Failed(_) => (l.original.clone(), egui::Color32::WHITE),
            };
            let original = (self.persisted.show_original
                && matches!(l.japanese, Japanese::Done(_) | Japanese::Pending))
            .then(|| format!("[{}] {}", l.lang, l.original));
            (main, color, original)
        });
        let mut exit = false;
        egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 14.0, egui::Color32::from_black_alpha((190.0 * alpha) as u8));
            let drag = ui.interact(rect, egui::Id::new("band-drag"), egui::Sense::click_and_drag());
            if drag.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if let Some((main, color, original)) = content {
                let width = rect.width() - 48.0;
                ui.add_space(14.0);
                ui.vertical_centered(|ui| {
                    ui.add(egui::Label::new(band_job(&main, size * 1.9, color.gamma_multiply(alpha), 2, width)).selectable(false));
                    if let Some(original) = original {
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(band_job(&original, size, APP_SHELL_WEAK_TEXT.gamma_multiply(alpha), 1, width))
                                .selectable(false),
                        );
                    }
                });
            }
            if alpha > 0.05 && ui.rect_contains_pointer(rect) {
                let button = egui::Rect::from_min_size(rect.right_top() + egui::vec2(-150.0, 6.0), egui::vec2(144.0, 24.0));
                if ui.put(button, egui::Button::new("元に戻す（ESC）").small()).clicked() {
                    exit = true;
                }
            }
        });
        if exit {
            self.exit_band(&ctx);
        }
    }

    fn subtitles(&self, ui: &mut egui::Ui) {
        let body = egui::TextStyle::Body.resolve(ui.style()).size;
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if self.lines.is_empty() {
                ui.colored_label(
                    APP_SHELL_WEAK_TEXT,
                    if self.running() { "音声を待っています…" } else { "「開始」を押すと、Mac で鳴っている音を字幕にします。" },
                );
            }
            for line in &self.lines {
                let big = |t: &str| egui::RichText::new(t).size(body * 1.3).strong();
                match &line.japanese {
                    Japanese::Done(ja) => {
                        ui.label(big(ja));
                    }
                    Japanese::Pending => {
                        ui.colored_label(APP_SHELL_WEAK_TEXT, "翻訳中…");
                    }
                    Japanese::Failed(e) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), format!("翻訳失敗: {e}"));
                    }
                    Japanese::NotNeeded => {
                        ui.label(big(&line.original));
                    }
                }
                if self.persisted.show_original && !matches!(line.japanese, Japanese::NotNeeded) {
                    ui.colored_label(APP_SHELL_WEAK_TEXT, format!("[{}] {}", line.lang, line.original));
                }
                ui.add_space(body * 0.6);
            }
        });
    }
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
    job.halign = egui::Align::Center;
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
            egui::Panel::top("controls").show_inside(ui, |ui| self.controls(ui));
            egui::CentralPanel::default().show_inside(ui, |ui| self.subtitles(ui));
        }
    }

    fn on_exit(&mut self) {
        // Stop asking Ollama for anything, then free its memory before the process goes away.
        translate::close_ollama();
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
fn install_safety_net() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = translate::ollama_unload_all();
        previous(info);
    }));
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                translate::close_ollama();
                translate::shutdown();
                let _ = translate::ollama_unload_all();
                std::process::exit(0);
            }
        });
    }
}

fn main() -> eframe::Result {
    install_safety_net();
    let options = eframe::NativeOptions {
        // The band changes the window geometry; do not let that become the next launch's normal window.
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 480.0])
            .with_min_inner_size(NORMAL_MIN_SIZE)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Live Subtitle", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
