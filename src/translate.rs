//! Translation backends: a local Ollama server or a long-lived Claude CLI session.

use serde::{Deserialize, Serialize};
use std::process::ChildStdout;
use std::sync::mpsc::Receiver;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Set while the app is shutting down: Ollama must not be asked for anything that would load a model again.
static OLLAMA_CLOSED: AtomicBool = AtomicBool::new(false);

pub fn close_ollama() {
    OLLAMA_CLOSED.store(true, Ordering::SeqCst);
}

fn ollama(model: &str, system: &str, text: &str) -> Result<String, String> {
    if OLLAMA_CLOSED.load(Ordering::SeqCst) {
        return Err("終了処理中".into());
    }
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

/// The models Ollama holds in memory right now. `None` means no server is listening: nothing is loaded,
/// and an idle server is never started just to ask.
fn loaded_models(agent: &ureq::Agent, host: &str) -> Result<Option<Vec<String>>, String> {
    let mut response = match agent.get(&format!("{host}/api/ps")).call() {
        Ok(response) => response,
        Err(ureq::Error::ConnectionFailed | ureq::Error::Io(_)) => return Ok(None),
        Err(e) => return Err(format!("Ollama の状態を取れない: {e}")),
    };
    if !response.status().is_success() {
        return Err(format!("Ollama の状態を取れない: {}", response.status()));
    }
    let value: serde_json::Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
    let mut names: Vec<String> = Vec::new();
    for model in value["models"].as_array().into_iter().flatten() {
        if let Some(name) = model["name"].as_str() {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    Ok(Some(names))
}

/// Unloads every model Ollama holds in memory, including ones other apps loaded, and returns how many.
///
/// Each loaded model gets `keep_alive: 0` (Ollama's way to unload it), then `/api/ps` is polled until they
/// are gone. A model somebody else loads in the meantime is reported instead of being chased.
pub fn ollama_unload_all() -> Result<usize, String> {
    unload_all_at(&ollama_host())
}

/// An agent that hands back error statuses as responses, so the server's own reason can be shown.
fn lenient_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().into()
}

fn unload_all_at(host: &str) -> Result<usize, String> {
    let agent = lenient_agent(Duration::from_secs(10));
    let Some(loaded) = loaded_models(&agent, host)? else {
        return Ok(0);
    };
    if loaded.is_empty() {
        return Ok(0);
    }
    let mut failures = Vec::new();
    for model in &loaded {
        let body = serde_json::json!({ "model": model, "keep_alive": 0, "stream": false });
        let outcome = agent.post(&format!("{host}/api/generate")).send_json(&body).map_err(|e| e.to_string()).and_then(|mut r| {
            let status = r.status();
            let reply = r.body_mut().read_json::<serde_json::Value>().unwrap_or_default();
            if !status.is_success() {
                return Err(format!("{status}: {}", reply["error"].as_str().unwrap_or("不明なエラー")));
            }
            if reply["done"].as_bool() == Some(true) {
                Ok(())
            } else {
                Err("解放が受け付けられなかった".to_string())
            }
        });
        if let Err(e) = outcome {
            failures.push(format!("{model}: {e}"));
        }
    }
    if !failures.is_empty() {
        return Err(format!("解放できなかったモデルがある: {}", failures.join("; ")));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let running = loaded_models(&agent, host)?.unwrap_or_default();
        if running.is_empty() {
            return Ok(loaded.len());
        }
        if running.iter().all(|name| !loaded.contains(name)) {
            return Err(format!("解放中に別のモデルが読み込まれた: {}", running.join(", ")));
        }
        if Instant::now() > deadline {
            return Err("モデルの解放が 30 秒で終わらなかった".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod unload_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A fake Ollama: `/api/ps` lists `loaded`; `/api/generate` with keep_alive 0 removes the model
    /// (or answers 400 for a model named "stuck"). Every POST body is recorded.
    struct Fake {
        host: String,
        loaded: Arc<Mutex<Vec<String>>>,
        posts: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    fn fake_ollama(initial: &[&str]) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let loaded = Arc::new(Mutex::new(initial.iter().map(|s| s.to_string()).collect::<Vec<_>>()));
        let posts = Arc::new(Mutex::new(Vec::new()));
        let (state, record) = (loaded.clone(), posts.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (state, record) = (state.clone(), record.clone());
                std::thread::spawn(move || serve(stream, &state, &record));
            }
        });
        Fake { host, loaded, posts }
    }

    fn serve(mut stream: std::net::TcpStream, loaded: &Mutex<Vec<String>>, posts: &Mutex<Vec<serde_json::Value>>) {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        let (head_end, length) = loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                return;
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..pos]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                    .unwrap_or(0);
                break (pos + 4, length);
            }
        };
        while data.len() < head_end + length {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
        }
        let request = String::from_utf8_lossy(&data[..head_end]).to_string();
        let (status, body) = if request.starts_with("GET /api/ps") {
            let models: Vec<_> = loaded.lock().unwrap().iter().map(|n| serde_json::json!({ "name": n })).collect();
            ("200 OK", serde_json::json!({ "models": models }))
        } else {
            let body: serde_json::Value = serde_json::from_slice(&data[head_end..]).unwrap_or_default();
            posts.lock().unwrap().push(body.clone());
            let name = body["model"].as_str().unwrap_or("").to_string();
            if name == "stuck" {
                ("400 Bad Request", serde_json::json!({ "error": "cannot unload" }))
            } else {
                loaded.lock().unwrap().retain(|n| *n != name);
                ("200 OK", serde_json::json!({ "done": true }))
            }
        };
        let body = body.to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    #[test]
    fn unloads_every_loaded_model_with_keep_alive_zero() {
        let fake = fake_ollama(&["big:55b", "small:7b"]);
        assert_eq!(unload_all_at(&fake.host), Ok(2));
        assert!(fake.loaded.lock().unwrap().is_empty());
        let posts = fake.posts.lock().unwrap();
        assert_eq!(posts.len(), 2);
        for (post, name) in posts.iter().zip(["big:55b", "small:7b"]) {
            assert_eq!(*post, serde_json::json!({ "model": name, "keep_alive": 0, "stream": false }));
        }
    }

    #[test]
    fn nothing_loaded_sends_no_unload_request() {
        let fake = fake_ollama(&[]);
        assert_eq!(unload_all_at(&fake.host), Ok(0));
        assert!(fake.posts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_model_that_cannot_be_unloaded_is_named_in_the_error() {
        let fake = fake_ollama(&["ok:1b", "stuck"]);
        let error = unload_all_at(&fake.host).unwrap_err();
        assert!(error.contains("stuck") && error.contains("cannot unload"), "{error}");
    }

    #[test]
    fn a_server_that_is_not_running_holds_no_models() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(unload_all_at(&format!("http://127.0.0.1:{port}")), Ok(0));
    }
}
