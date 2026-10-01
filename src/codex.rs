//! Translation through a long-lived `codex app-server` process (JSON-RPC over stdio).
//!
//! One server process stays running and hosts `SLOTS` ephemeral threads, so several subtitle lines can be
//! translated at the same time: a single Codex turn takes a few seconds, which is slower than people talk.
//! A reader thread routes every incoming message to whoever is waiting for it (responses by request id,
//! notifications by thread id). A slot's thread is replaced after `NEW_THREAD_AFTER` turns so its history does
//! not grow without bound. Authentication is the signed-in ChatGPT account of the local Codex.

use crate::translate::{lock_within, subtitle_message, SUBTITLE_PROMPT, QUEUE_LIMIT};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

const SLOTS: usize = 6;
const NEW_THREAD_AFTER: usize = 40;
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// A model the signed-in account can use, as reported by `model/list`.
#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// Reasoning efforts this model accepts (an unsupported one makes the turn fail silently).
    pub efforts: Vec<String>,
}

pub fn codex_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LIVE_SUBTITLE_CODEX") {
        return Some(p.into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        home.map(|h| h.join(".local/bin/codex")),
        Some("/opt/homebrew/bin/codex".into()),
        Some("/usr/local/bin/codex".into()),
        Some("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex".into()),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

type Waiters = Arc<Mutex<HashMap<i64, Sender<Value>>>>;
type Routes = Arc<Mutex<HashMap<String, Sender<Value>>>>;

struct Server {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    /// Responses, by request id.
    waiters: Waiters,
    /// Notifications, by thread id.
    routes: Routes,
    next_id: AtomicI64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_line(stdin: &Mutex<ChildStdin>, message: &Value) -> Result<(), String> {
    let mut stdin = stdin.lock().map_err(|_| "codex への書き込みが壊れた".to_string())?;
    writeln!(stdin, "{message}")
        .and_then(|()| stdin.flush())
        .map_err(|e| format!("codex に送れない: {e}"))
}

impl Server {
    /// Starts `codex app-server` and completes the initialize handshake.
    fn start() -> Result<Self, String> {
        let bin = codex_binary().ok_or("codex コマンドが見つからない（LIVE_SUBTITLE_CODEX で指定できる）")?;
        let mut child = Command::new(bin)
            .arg("app-server")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("codex を起動できない: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or("stdin を開けない")?));
        let stdout = child.stdout.take().ok_or("stdout を開けない")?;
        let waiters: Waiters = Arc::default();
        let routes: Routes = Arc::default();
        {
            let (stdin, waiters, routes) = (stdin.clone(), waiters.clone(), routes.clone());
            std::thread::spawn(move || route_messages(stdout, &stdin, &waiters, &routes));
        }
        let server = Self { child, stdin, waiters, routes, next_id: AtomicI64::new(0) };
        server.request(
            "initialize",
            json!({ "clientInfo": { "name": "live_subtitle", "title": "Live Subtitle", "version": "0.1.0" } }),
        )?;
        write_line(&server.stdin, &json!({ "method": "initialized", "params": {} }))?;
        Ok(server)
    }

    /// Sends a request and waits for its response.
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::channel();
        self.waiters.lock().map_err(|_| "codex の状態が壊れた".to_string())?.insert(id, tx);
        write_line(&self.stdin, &json!({ "method": method, "id": id, "params": params }))?;
        let event = match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => return Err(format!("{method} がタイムアウト")),
            Err(RecvTimeoutError::Disconnected) => return Err("codex が終了した".into()),
        };
        match event.get("error") {
            Some(error) => Err(format!("{method}: {}", error["message"].as_str().unwrap_or("エラー"))),
            None => Ok(event["result"].clone()),
        }
    }
}

/// Reads the server's output and hands each message to its waiter. Requests the server makes of us
/// (approvals and the like) are declined, since translation never needs a tool.
fn route_messages(stdout: std::process::ChildStdout, stdin: &Mutex<ChildStdin>, waiters: &Waiters, routes: &Routes) {
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let has_method = event.get("method").is_some();
        if has_method && event.get("id").is_some() {
            let reply = json!({ "id": event["id"], "error": { "code": -32601, "message": "not supported" } });
            let _ = write_line(stdin, &reply);
        } else if !has_method {
            if let Some(id) = event["id"].as_i64() {
                let tx = waiters.lock().ok().and_then(|mut w| w.remove(&id));
                if let Some(tx) = tx {
                    let _ = tx.send(event);
                }
            }
        } else {
            // Notifications about a thread go to its slot; the rest (account events) go to the "" route.
            let key = event["params"]["threadId"].as_str().unwrap_or("").to_string();
            let tx = routes.lock().ok().and_then(|r| r.get(&key).cloned());
            if let Some(tx) = tx {
                let _ = tx.send(event);
            }
        }
    }
    // The process ended: drop every sender so anyone still waiting stops waiting.
    if let Ok(mut w) = waiters.lock() {
        w.clear();
    }
    if let Ok(mut r) = routes.lock() {
        r.clear();
    }
}

/// One ephemeral thread and the channel its notifications arrive on.
struct Slot {
    thread_id: String,
    events: Receiver<Value>,
    turns: usize,
}

struct Pool {
    server: Server,
    slots: Vec<Mutex<Slot>>,
    model: String,
    effort: String,
    next: AtomicUsize,
}

fn start_thread(server: &Server, model: &str) -> Result<Slot, String> {
    let mut params = json!({
        "ephemeral": true,
        "approvalPolicy": "never",
        "sandbox": "read-only",
        "baseInstructions": SUBTITLE_PROMPT,
        "cwd": std::env::temp_dir().to_string_lossy(),
    });
    if !model.is_empty() {
        params["model"] = json!(model);
    }
    let result = server.request("thread/start", params)?;
    let thread_id = result["thread"]["id"].as_str().ok_or("thread/start に id が無い")?.to_string();
    let (tx, events) = mpsc::channel();
    server.routes.lock().map_err(|_| "codex の状態が壊れた".to_string())?.insert(thread_id.clone(), tx);
    Ok(Slot { thread_id, events, turns: 0 })
}

impl Pool {
    fn open(model: &str, effort: &str) -> Result<Self, String> {
        let server = Server::start()?;
        let slots = (0..SLOTS)
            .map(|_| start_thread(&server, model).map(Mutex::new))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { server, slots, model: model.to_string(), effort: effort.to_string(), next: AtomicUsize::new(0) })
    }

    /// A free slot, waiting up to `QUEUE_LIMIT` for one.
    fn free_slot(&self) -> Result<MutexGuard<'_, Slot>, String> {
        let deadline = Instant::now() + QUEUE_LIMIT;
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        loop {
            for i in 0..SLOTS {
                match self.slots[(start + i) % SLOTS].try_lock() {
                    Ok(guard) => return Ok(guard),
                    Err(TryLockError::Poisoned(_)) => return Err("codex の状態が壊れた".into()),
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            if Instant::now() > deadline {
                return Err("翻訳が追いつかない".into());
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn ask(&self, slot: &mut Slot, context: &[String], lang: &str, text: &str) -> Result<String, String> {
        if slot.turns >= NEW_THREAD_AFTER {
            let old = slot.thread_id.clone();
            *slot = start_thread(&self.server, &self.model)?;
            self.server.routes.lock().map_err(|_| "codex の状態が壊れた".to_string())?.remove(&old);
        }
        slot.turns += 1;
        while slot.events.try_recv().is_ok() {} // leftovers of an earlier turn

        let message = subtitle_message(context, lang, text);
        let mut params = json!({ "threadId": slot.thread_id, "input": [{ "type": "text", "text": message }] });
        if !self.effort.is_empty() {
            params["effort"] = json!(self.effort);
        }
        self.server.request("turn/start", params)?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut reply = String::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = match slot.events.recv_timeout(left) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => return Err("codex がタイムアウト".into()),
                Err(RecvTimeoutError::Disconnected) => return Err("codex が終了した".into()),
            };
            match event["method"].as_str() {
                Some("item/agentMessage/delta") => {
                    if let Some(piece) = event["params"]["delta"].as_str() {
                        reply.push_str(piece);
                    }
                }
                Some("item/completed") => {
                    let item = &event["params"]["item"];
                    if reply.trim().is_empty() && item["type"] == "agentMessage" {
                        if let Some(text) = item["text"].as_str() {
                            reply.push_str(text);
                        }
                    }
                }
                Some("turn/completed") => {
                    let status = event["params"]["turn"]["status"].as_str().unwrap_or("");
                    return if status == "completed" && !reply.trim().is_empty() {
                        Ok(reply)
                    } else {
                        let detail = event["params"]["turn"]["error"]["message"].as_str().unwrap_or("訳が空だった");
                        Err(format!("codex のターンが失敗（{status}）: {detail}"))
                    };
                }
                _ => {}
            }
        }
    }
}

static POOL: Mutex<Option<Arc<Pool>>> = Mutex::new(None);

/// Translates one subtitle line; up to `SLOTS` lines are translated at the same time.
pub fn translate(model: &str, effort: &str, context: &[String], lang: &str, text: &str) -> Result<String, String> {
    let pool = {
        let mut current = lock_within(&POOL, QUEUE_LIMIT)?;
        if current.as_ref().is_none_or(|p| p.model != model || p.effort != effort) {
            *current = None;
            *current = Some(Arc::new(Pool::open(model, effort)?));
        }
        current.clone().ok_or("codex のセッションが無い")?
    };
    let mut slot = pool.free_slot()?;
    match pool.ask(&mut slot, context, lang, text) {
        Ok(reply) => Ok(reply),
        Err(e) => {
            // Start over with a fresh process next time, unless someone already replaced this pool.
            drop(slot);
            if let Ok(mut current) = POOL.lock() {
                if current.as_ref().is_some_and(|p| Arc::ptr_eq(p, &pool)) {
                    *current = None;
                }
            }
            Err(e)
        }
    }
}

/// Stops the running server.
pub fn shutdown() {
    if let Ok(mut current) = POOL.lock() {
        *current = None;
    }
}

/// Whether the local Codex is signed in to an account.
pub fn signed_in() -> Result<bool, String> {
    let server = Server::start()?;
    let result = server.request("account/read", json!({}))?;
    Ok(!result["account"].is_null())
}

/// Starts the ChatGPT sign-in: opens the authorization page in the browser and waits for the user to finish
/// there. The server must stay alive meanwhile because it receives the browser's callback.
pub fn sign_in() -> Result<(), String> {
    let server = Server::start()?;
    let (tx, completed) = mpsc::channel();
    server.routes.lock().map_err(|_| "codex の状態が壊れた".to_string())?.insert(String::new(), tx);
    let started = server.request("account/login/start", json!({ "type": "chatgpt" }))?;
    let url = started["authUrl"].as_str().ok_or("サインイン用の URL が返らなかった")?;
    Command::new("open").arg(url).spawn().map_err(|e| format!("ブラウザを開けない: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = completed.recv_timeout(left).map_err(|_| "サインインが完了しなかった".to_string())?;
        if event["method"] == "account/login/completed" {
            return if event["params"]["success"].as_bool().unwrap_or(false) {
                Ok(())
            } else {
                Err(event["params"]["error"].as_str().unwrap_or("サインインに失敗した").to_string())
            };
        }
    }
}

/// Asks a short-lived server which models the signed-in account can use.
pub fn list_models() -> Result<Vec<ModelInfo>, String> {
    let server = Server::start()?;
    let result = server.request("model/list", json!({}))?;
    let models = result["data"].as_array().ok_or("model/list の応答が不正")?;
    Ok(models
        .iter()
        .filter(|m| !m["hidden"].as_bool().unwrap_or(false))
        .filter_map(|m| {
            Some(ModelInfo {
                id: m["id"].as_str()?.to_string(),
                name: m["displayName"].as_str().unwrap_or_else(|| m["id"].as_str().unwrap_or("")).to_string(),
                efforts: m["supportedReasoningEfforts"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|e| e["reasoningEffort"].as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            })
        })
        .collect())
}
