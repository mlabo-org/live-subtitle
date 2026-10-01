//! Translation backends: a local Ollama server or a long-lived Claude CLI session.

use serde::{Deserialize, Serialize};
use std::process::ChildStdout;
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// Claude models offered in the window: an explicit version, a short label. Aliases such as "haiku" are
/// avoided because the version they point at changes with the CLI.
pub const CLAUDE_MODELS: [(&str, &str); 4] = [
    ("claude-haiku-4-5-20251001", "Haiku 4.5（最速・約0.7秒）"),
    ("claude-sonnet-5-5", "Sonnet 5.5（自然・約1.7秒）"),
    ("claude-opus-5-5", "Opus 5.5（自然・約2秒）"),
    ("claude-fable-5-1", "Fable 5.1（高品質・約0.8秒）"),
];

/// Settings saved by earlier builds stored an alias; this returns the explicit model it stood for.
pub fn explicit_claude_model(stored: &str) -> String {
    match stored {
        "haiku" => CLAUDE_MODELS[0].0,
        "sonnet" => CLAUDE_MODELS[1].0,
        "opus" => CLAUDE_MODELS[2].0,
        other => other,
    }
    .to_string()
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    Ollama,
    Claude,
    Codex,
    Off,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Ollama => "Ollama（ローカル・速い）",
            Engine::Claude => "Claude（高品質）",
            Engine::Codex => "Codex（ChatGPT アカウント）",
            Engine::Off => "翻訳しない（原文のみ）",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateSettings {
    pub engine: Engine,
    pub ollama_model: String,
    pub claude_model: String,
    /// A model id from Codex's `model/list`; the account's default model is `gpt-6.1-sol`.
    pub codex_model: String,
    /// Reasoning effort for Codex; empty means the model's own default.
    pub codex_effort: String,
}

impl Default for TranslateSettings {
    fn default() -> Self {
        Self {
            engine: Engine::Ollama,
            ollama_model: "gemma4:26b-mlx".into(),
            claude_model: CLAUDE_MODELS[0].0.into(),
            codex_model: "gpt-6.1-sol".into(),
            codex_effort: String::new(),
        }
    }
}

/// System prompt of the long-lived Claude and Codex sessions, where each message is one subtitle line.
pub const SUBTITLE_PROMPT: &str = "You are a live subtitle translator. Every user message is one subtitle line written as \
`[source-language-code] text`, sometimes after a `Context` list of preceding lines. Reply with only the natural spoken \
Japanese translation of the `[code] text` line: no notes, no quotation marks, no language tag; never translate the \
context lines. Keep proper nouns recognizable. If the text is already Japanese, repeat it unchanged. Earlier messages \
are earlier subtitle lines; use them for context only.";

/// Reads a child's stdout line by line on its own thread so callers can wait with a timeout.
pub fn spawn_line_reader(stdout: ChildStdout) -> Receiver<String> {
    use std::io::{BufRead, BufReader};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// How long a subtitle may wait for a free translator before it is shown untranslated, so the
/// subtitles never fall further and further behind the speech.
pub const QUEUE_LIMIT: Duration = Duration::from_secs(10);

/// Locks `mutex`, giving up with a "can't keep up" error after `QUEUE_LIMIT`.
pub fn lock_within<T>(mutex: &Mutex<T>, wait: Duration) -> Result<MutexGuard<'_, T>, String> {
    let deadline = Instant::now() + wait;
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err("翻訳の状態が壊れた".into()),
            Err(TryLockError::WouldBlock) if Instant::now() > deadline => return Err("翻訳が追いつかない".into()),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(15)),
        }
    }
}

/// Stops the long-lived Claude and Codex processes.
pub fn shutdown() {
    crate::claude::shutdown();
    crate::codex::shutdown();
}

const SYSTEM_PROMPT: &str = "You are a live subtitle translator. Translate the user's text into natural spoken Japanese. \
Output only the Japanese translation, with no notes or quotation marks. \
Keep proper nouns recognizable. If the text is already Japanese, output it unchanged.";

fn system_prompt(lang: &str, context: &[String]) -> String {
    let mut p = format!("{SYSTEM_PROMPT}\nSource language code: {lang}.");
    if !context.is_empty() {
        p.push_str("\nPrevious lines, for context only (do not translate them):\n");
        for c in context {
            p.push_str("- ");
            p.push_str(c);
            p.push('\n');
        }
    }
    p
}

pub fn translate(
    settings: &TranslateSettings,
    text: &str,
    lang: &str,
    context: &[String],
) -> Result<String, String> {
    let system = system_prompt(lang, context);
    let out = match settings.engine {
        Engine::Ollama => ollama(&settings.ollama_model, &system, text)?,
        Engine::Claude => crate::claude::translate(&settings.claude_model, lang, text)?,
        Engine::Codex => crate::codex::translate(&settings.codex_model, &settings.codex_effort, context, lang, text)?,
        Engine::Off => return Ok(text.to_string()),
    };
    let out = out.trim().to_string();
    if out.is_empty() {
        Err("翻訳結果が空".into())
    } else {
        Ok(out)
    }
}

fn ollama_host() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .map(|h| if h.starts_with("http") { h } else { format!("http://{h}") })
        .unwrap_or_else(|| "http://127.0.0.1:11434".into())
}

fn ollama_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn ollama(model: &str, system: &str, text: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "stream": false,
        "think": false,
        "keep_alive": "30m",
        "options": { "temperature": 0, "num_predict": 400 },
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": text },
        ],
    });
    let mut resp = ollama_agent(Duration::from_secs(90))
        .post(&format!("{}/api/chat", ollama_host()))
        .send_json(&body)
        .map_err(|e| format!("Ollama に接続できない: {e}"))?;
    let v: serde_json::Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    v["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("Ollama の応答が不正: {v}"))
}

pub fn ollama_models() -> Vec<String> {
    let Ok(mut resp) = ollama_agent(Duration::from_secs(3)).get(&format!("{}/api/tags", ollama_host())).call() else {
        return Vec::new();
    };
    let Ok(v) = resp.body_mut().read_json::<serde_json::Value>() else {
        return Vec::new();
    };
    v["models"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}
