//! Translation through long-lived `claude -p` processes (stream-json in and out).
//!
//! Starting `claude` takes seconds, so a process is kept running and every subtitle line is one more
//! message to it. Extended thinking is turned off where the model allows it (Haiku 5.5 always thinks
//! briefly), and the reply is taken as soon as its stream ends instead of waiting for the final result message. One process translates one line at a time, which is slower than
//! people talk, so up to `SESSIONS` of them run side by side; the second and third start only when a line
//! finds the others busy. The conversation grows with every message, so a session is replaced after
//! `RECYCLE_AFTER` lines; the replacement is prepared in the background beforehand.

use serde_json::{json, Value};
use crate::translate::{spawn_line_reader, subtitle_message, QUEUE_LIMIT, SUBTITLE_PROMPT};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

const SESSIONS: usize = 3;
const RECYCLE_AFTER: usize = 40;
const PREPARE_AT: usize = 30;
const REPLY_TIMEOUT: Duration = Duration::from_secs(45);

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    model: String,
    requests: usize,
    /// Messages whose final `result` line has not been read yet (it trails the reply by a moment).
    outstanding: usize,
}

static SLOTS: [Mutex<Option<Session>>; SESSIONS] = [const { Mutex::new(None) }; SESSIONS];
static SPARE: Mutex<Option<Session>> = Mutex::new(None);
static PREPARING: AtomicBool = AtomicBool::new(false);

pub fn claude_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LIVE_SUBTITLE_CLAUDE") {
        return Some(p.into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        home.map(|h| h.join(".local/bin/claude")),
        Some("/opt/homebrew/bin/claude".into()),
        Some("/usr/local/bin/claude".into()),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    /// Starts the process and sends one greeting so the first real subtitle is not slowed by the cold start.
    fn spawn(model: &str) -> Result<Self, String> {
        let bin = claude_binary().ok_or("claude コマンドが見つからない（LIVE_SUBTITLE_CLAUDE で指定できる）")?;
        let mut child = Command::new(bin)
            .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json"])
            .args(["--verbose", "--include-partial-messages", "--thinking", "disabled", "--model", model])
            .args(["--tools", "", "--setting-sources", "", "--strict-mcp-config"])
            .args(["--disable-slash-commands", "--no-session-persistence", "--system-prompt", SUBTITLE_PROMPT])
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("claude を起動できない: {e}"))?;
        let stdin = child.stdin.take().ok_or("stdin を開けない")?;
        let stdout = child.stdout.take().ok_or("stdout を開けない")?;
        let lines = spawn_line_reader(stdout);
        let mut session = Self { child, stdin, lines, model: model.to_string(), requests: 0, outstanding: 0 };
        session.ask(&subtitle_message(&[], "en", "Hello."))?;
        Ok(session)
    }

    fn ask(&mut self, content: &str) -> Result<String, String> {
        let message = json!({
            "type": "user",
            "message": { "role": "user", "content": content },
        });
        writeln!(self.stdin, "{message}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("claude に送れない: {e}"))?;
        self.requests += 1;
        self.outstanding += 1;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut reply = String::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return Err("claude がタイムアウト".into()),
                Err(RecvTimeoutError::Disconnected) => return Err("claude が終了した".into()),
            };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match event["type"].as_str() {
                Some("stream_event") => match event["event"]["type"].as_str() {
                    Some("content_block_delta") => {
                        if let Some(piece) = event["event"]["delta"]["text"].as_str() {
                            reply.push_str(piece);
                        }
                    }
                    Some("message_stop") if !reply.trim().is_empty() => return Ok(reply),
                    _ => {}
                },
                Some("result") => {
                    self.outstanding = self.outstanding.saturating_sub(1);
                    // The result of an earlier message is read while waiting; only the current one ends the wait.
                    if self.outstanding == 0 {
                        return if reply.trim().is_empty() {
                            Err(event["result"].as_str().unwrap_or("空の応答").to_string())
                        } else {
                            Ok(reply)
                        };
                    }
                }
                _ => {}
            }
        }
    }
}

fn take_spare(model: &str) -> Option<Session> {
    let spare = SPARE.lock().ok()?.take()?;
    (spare.model == model).then_some(spare)
}

fn prepare_spare(model: &str) {
    if PREPARING.swap(true, Ordering::SeqCst) {
        return;
    }
    let model = model.to_string();
    std::thread::spawn(move || {
        if let Ok(session) = Session::spawn(&model) {
            if let Ok(mut spare) = SPARE.lock() {
                *spare = Some(session);
            }
        }
        PREPARING.store(false, Ordering::SeqCst);
    });
}

/// A slot nobody is using, waiting up to `QUEUE_LIMIT` for one. A slot whose session already runs `model` is
/// preferred, so unhurried speech stays with one process and the others start only under load.
fn free_slot(model: &str) -> Result<MutexGuard<'static, Option<Session>>, String> {
    let deadline = Instant::now() + QUEUE_LIMIT;
    loop {
        let mut idle = None;
        for slot in &SLOTS {
            match slot.try_lock() {
                Ok(guard) if guard.as_ref().is_some_and(|s| s.model == model) => return Ok(guard),
                Ok(guard) => {
                    idle.get_or_insert(guard);
                }
                Err(TryLockError::Poisoned(_)) => return Err("翻訳の状態が壊れた".into()),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        if let Some(guard) = idle {
            return Ok(guard);
        }
        if Instant::now() > deadline {
            return Err("翻訳が追いつかない".into());
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// The slot's session for `model`, started first when none is running, or when the running one is for another
/// model or due for replacement.
fn current<'a>(active: &'a mut Option<Session>, model: &str) -> Result<&'a mut Session, String> {
    if active.as_ref().is_none_or(|s| s.model != model || s.requests >= RECYCLE_AFTER) {
        *active = None;
        *active = Some(match take_spare(model) {
            Some(spare) => spare,
            None => Session::spawn(model)?,
        });
    }
    active.as_mut().ok_or_else(|| "claude のセッションが無い".to_string())
}

/// Starts one session for `model` ahead of the first subtitle (starting one already sends the greeting) and
/// stops idle sessions left over from another model.
pub fn warm_up(model: &str) -> Result<(), String> {
    for slot in &SLOTS {
        if let Ok(mut session) = slot.try_lock() {
            if session.as_ref().is_some_and(|s| s.model != model) {
                *session = None;
            }
        }
    }
    let mut active = free_slot(model)?;
    current(&mut active, model).map(|_| ())
}

/// Translates one subtitle line; up to `SESSIONS` lines are translated at the same time.
pub fn translate(model: &str, context: &[String], lang: &str, text: &str) -> Result<String, String> {
    let mut active = free_slot(model)?;
    let session = current(&mut active, model)?;
    if session.requests == PREPARE_AT {
        prepare_spare(model);
    }
    match session.ask(&subtitle_message(context, lang, text)) {
        Ok(reply) => Ok(reply),
        Err(e) => {
            *active = None; // start over with a fresh process next time
            Err(e)
        }
    }
}

/// Stops the running sessions and any prepared replacement.
pub fn shutdown() {
    for slot in &SLOTS {
        if let Ok(mut session) = slot.lock() {
            *session = None;
        }
    }
    if let Ok(mut spare) = SPARE.lock() {
        *spare = None;
    }
}

/// Runs `claude` with `args` and returns its stdout, giving up after `timeout`.
fn run_capture(args: &[&str], timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    let bin = claude_binary().ok_or("claude コマンドが見つからない（LIVE_SUBTITLE_CLAUDE で指定できる）")?;
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("claude を起動できない: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("stdout を開けない")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => return reader.join().map_err(|_| "読み取りに失敗".to_string()),
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return Err("claude がタイムアウト".into());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Whether the local Claude Code is signed in (`claude auth status`, read-only).
pub fn signed_in() -> Result<bool, String> {
    let text = run_capture(&["auth", "status", "--json"], Duration::from_secs(20))?;
    parse_signed_in(&text)
}

fn parse_signed_in(status_json: &str) -> Result<bool, String> {
    let value: Value = serde_json::from_str(status_json).map_err(|e| format!("状態を読めない: {e}"))?;
    value["loggedIn"].as_bool().ok_or_else(|| "状態に loggedIn が無い".to_string())
}

/// Starts the official `claude auth login` (Claude subscription, browser OAuth) and waits until the
/// status turns signed-in, for up to five minutes.
pub fn sign_in() -> Result<(), String> {
    let bin = claude_binary().ok_or("claude コマンドが見つからない（LIVE_SUBTITLE_CLAUDE で指定できる）")?;
    let mut child = Command::new(bin)
        .args(["auth", "login"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("claude を起動できない: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        let exited = child.try_wait().map_err(|e| e.to_string())?.is_some();
        if signed_in().unwrap_or(false) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        if exited {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Err("サインインが完了しなかった。ターミナルで `claude auth login` を実行してほしい".into())
}

#[cfg(test)]
mod tests {
    use super::parse_signed_in;

    #[test]
    fn reads_logged_in_from_status_json() {
        assert_eq!(parse_signed_in(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#), Ok(true));
        assert_eq!(parse_signed_in(r#"{"loggedIn": false}"#), Ok(false));
        assert!(parse_signed_in("not json").is_err());
        assert!(parse_signed_in("{}").is_err());
    }
}
