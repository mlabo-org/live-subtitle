#[allow(dead_code)]
mod app_shell_foundation;
mod agc;
mod capture;
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
use translate::{Engine, TranslateSettings};

const APP_SHELL_STORAGE_KEY: &str = "live-subtitle.app-shell-preferences.v1";
const SETTINGS_STORAGE_KEY: &str = "live-subtitle.settings.v1";
const MAX_LINES: usize = 500;
const METER_MIN_DB: f32 = -70.0;
const METER_SILENCE_DB: f32 = -60.0;
const METER_LOUD_DB: f32 = -25.0;
const METER_GREEN: egui::Color32 = egui::Color32::from_rgb(52, 199, 89);
const METER_YELLOW: egui::Color32 = egui::Color32::from_rgb(255, 204, 0);

#[derive(Clone, Serialize, Deserialize)]
struct Persisted {
    translate: TranslateSettings,
    always_on_top: bool,
    show_original: bool,
}

impl Default for Persisted {
    fn default() -> Self {
        Self { translate: TranslateSettings::default(), always_on_top: true, show_original: true }
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
    lang: String,
    original: String,
    japanese: Japanese,
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_macos_system_fonts(&cc.egui_ctx).expect("the managed macOS UI font must be available");
        let preferences = load_app_shell_preferences(cc.storage, APP_SHELL_STORAGE_KEY);
        apply_app_shell_preferences(&cc.egui_ctx, preferences);
        let persisted: Persisted = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_STORAGE_KEY))
            .unwrap_or_default();
        cc.egui_ctx.send_viewport_cmd(level_command(persisted.always_on_top));
        let (tx, rx) = mpsc::channel();
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
        };
        if autostart {
            app.start(&cc.egui_ctx);
        }
        app
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
        self.asr = Stage::Idle;
        self.translator = Stage::Idle;
        self.level = 0.0;
        self.gain_db = 0.0;
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if !self.running() && !matches!(ev, Event::Translated { .. } | Event::TranslateFailed { .. }) {
                continue;
            }
            match ev {
                Event::Asr(s) => self.asr = s,
                Event::Translator(s) => self.translator = s,
                Event::Level { rms, gain_db } => {
                    self.level = rms.max(self.level * 0.95);
                    self.gain_db = gain_db;
                }
                Event::Heard { id, lang, text } => {
                    let engine = self.persisted.translate.engine;
                    let japanese = if lang == "ja" || engine == Engine::Off {
                        Japanese::NotNeeded
                    } else {
                        Japanese::Pending
                    };
                    self.lines.push(Line { id, lang, original: text, japanese });
                    if self.lines.len() > MAX_LINES {
                        self.lines.remove(0);
                    }
                }
                Event::Translated { id, text } => self.set_japanese(id, Japanese::Done(text)),
                Event::TranslateFailed { id, error } => self.set_japanese(id, Japanese::Failed(error)),
                Event::Fatal(e) => {
                    self.error = Some(e);
                    self.stop();
                }
            }
        }
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
        if self.running() && self.asr == Stage::Ready {
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
            let mut changed = false;
            let before_engine = self.persisted.translate.engine;
            let before_model = self.persisted.translate.ollama_model.clone();
            ui.label("翻訳先");
            egui::ComboBox::from_id_salt("engine")
                .selected_text(self.persisted.translate.engine.label())
                .show_ui(ui, |ui| {
                    for e in [Engine::Ollama, Engine::Claude, Engine::Off] {
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
                }
                Engine::Claude => {
                    let t = &mut self.persisted.translate;
                    egui::ComboBox::from_id_salt("claude-model")
                        .selected_text(t.claude_model.clone())
                        .show_ui(ui, |ui| {
                            for n in ["haiku", "sonnet", "opus"] {
                                changed |= ui.selectable_value(&mut t.claude_model, n.to_string(), n).changed();
                            }
                        });
                }
                Engine::Off => {}
            }
            if changed {
                if let Ok(mut s) = self.shared.lock() {
                    *s = self.persisted.translate.clone();
                }
                let t = &self.persisted.translate;
                let reload = t.engine == Engine::Ollama
                    && (before_engine != Engine::Ollama || before_model != t.ollama_model);
                if self.running() && reload {
                    let ctx = ui.ctx().clone();
                    pipeline::warm_up(self.shared.clone(), self.tx.clone(), move || ctx.request_repaint());
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.checkbox(&mut self.persisted.always_on_top, "最前面に固定").changed() {
                ui.ctx().send_viewport_cmd(level_command(self.persisted.always_on_top));
            }
            ui.checkbox(&mut self.persisted.show_original, "原文も表示");
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

fn level_command(on_top: bool) -> egui::ViewportCommand {
    egui::ViewportCommand::WindowLevel(if on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    })
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        egui::Panel::top("controls").show_inside(ui, |ui| self.controls(ui));
        egui::CentralPanel::default().show_inside(ui, |ui| self.subtitles(ui));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        save_app_shell_preferences(storage, APP_SHELL_STORAGE_KEY, &self.preferences);
        eframe::set_value(storage, SETTINGS_STORAGE_KEY, &self.persisted);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 480.0])
            .with_min_inner_size([380.0, 260.0]),
        ..Default::default()
    };
    eframe::run_native("Live Subtitle", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
