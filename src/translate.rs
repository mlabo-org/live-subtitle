//! Translation backends: a local Ollama server (fast) or the Claude CLI (high quality, slower).

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    Ollama,
    Claude,
    Off,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Ollama => "Ollama（ローカル・速い）",
            Engine::Claude => "Claude（高品質・遅い）",
            Engine::Off => "翻訳しない（原文のみ）",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TranslateSettings {
    pub engine: Engine,
    pub ollama_model: String,
    pub claude_model: String,
}

impl Default for TranslateSettings {
    fn default() -> Self {
        Self {
            engine: Engine::Ollama,
            ollama_model: "gemma4:26b-mlx".into(),
            claude_model: "haiku".into(),
        }
    }
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
        Engine::Claude => claude(&settings.claude_model, &system, text)?,
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

fn claude_binary() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("LIVE_SUBTITLE_CLAUDE") {
        return Some(p.into());
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    [
        home.map(|h| h.join(".local/bin/claude")),
        Some("/opt/homebrew/bin/claude".into()),
        Some("/usr/local/bin/claude".into()),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

fn claude(model: &str, system: &str, text: &str) -> Result<String, String> {
    let bin = claude_binary().ok_or("claude コマンドが見つからない（LIVE_SUBTITLE_CLAUDE で指定できる）")?;
    let mut child = Command::new(bin)
        .args(["-p", "--model", model, "--tools", "", "--setting-sources", ""])
        .args(["--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence"])
        .args(["--system-prompt", system])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("claude を起動できない: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("stdin を開けない")?
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("stdout を開けない")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return reader.join().map_err(|_| "読み取り失敗".to_string()),
            Some(status) => return Err(format!("claude が失敗: {status}")),
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return Err("claude がタイムアウト".into());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
