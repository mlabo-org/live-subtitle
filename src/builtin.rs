//! The built-in translation engine: llama.cpp's server, shipped inside the app and run as a child process on a
//! local port. Nothing else has to be installed; the model is a GGUF file found on disk or downloaded.

use crate::translate::{lock_within, QUEUE_LIMIT};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A model file as its publisher distributes it.
pub struct ModelFile {
    pub file: &'static str,
    /// The Hugging Face repository and the revision the size and checksum below belong to.
    repo: &'static str,
    revision: &'static str,
    pub size: u64,
    sha256: &'static str,
}

/// The model the engine uses unless the user chose a file: Google's Gemma 4 26B, trained to be quantized to
/// 4 bits (Apache 2.0, no sign-in needed). Compared on 40 spoken-style sentences it translated as well as the
/// Ollama MLX build of the same model, about 0.1 s per sentence faster.
pub const RECOMMENDED: ModelFile = ModelFile {
    file: "gemma-4-26B_q4_0-it.gguf",
    repo: "google/gemma-4-26B-A4B-it-qat-q4_0-gguf",
    revision: "d1c082be9cf3c8a514acf63b8761f4b41935842e",
    size: 14_439_363_584,
    sha256: "3eca3b8f6d7baf218a7dd6bba5fb59a56ee25fe2d567b6f5f589b4f697eca51d",
};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn data_dir() -> PathBuf {
    home().join("Library/Application Support/LiveSubtitle")
}

/// Where the tools that fetch models keep them, each as the tool itself resolves it.
struct Stores {
    /// The app's own folder, where its download goes.
    app: PathBuf,
    /// The Hugging Face cache, which current llama.cpp builds share. A large file is a blob named after its
    /// SHA-256, linked from `snapshots/`.
    hugging_face: PathBuf,
    /// Ollama's models: `manifests/` name the blobs of each model, and a blob is named `sha256-<digest>`.
    ollama: PathBuf,
    /// Folders that hold model files under their own names: the tool, and its folder.
    folders: Vec<(&'static str, PathBuf)>,
}

fn stores() -> Stores {
    let home = home();
    let hugging_face = match (std::env::var_os("HF_HUB_CACHE"), std::env::var_os("HF_HOME")) {
        (Some(dir), _) => dir.into(),
        (None, Some(dir)) => PathBuf::from(dir).join("hub"),
        (None, None) => home.join(".cache/huggingface/hub"),
    };
    let ollama = std::env::var_os("OLLAMA_MODELS").map_or_else(|| home.join(".ollama/models"), PathBuf::from);
    // LM Studio's folder, and the one its earlier versions used; llama.cpp's own cache from before it shared
    // the Hugging Face one.
    let llama_cpp = std::env::var_os("LLAMA_CACHE").map_or_else(|| home.join("Library/Caches/llama.cpp"), PathBuf::from);
    let folders = vec![
        ("LM Studio", home.join(".lmstudio/models")),
        ("LM Studio", home.join(".cache/lm-studio/models")),
        ("llama.cpp", llama_cpp),
    ];
    Stores { app: data_dir(), hugging_face, ollama, folders }
}

/// Calls `visit` for every file under `dir`, at most `depth` folders down. Links are followed (the Hugging
/// Face cache is made of them); the depth limit is what ends a loop of links.
fn each_file(dir: &Path, depth: usize, visit: &mut dyn FnMut(&Path, u64)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.metadata() else {
            continue;
        };
        if meta.is_file() {
            visit(&path, meta.len());
        } else if meta.is_dir() && depth > 0 {
            each_file(&path, depth - 1, visit);
        }
    }
}

/// How deep a model file sits in a tool's folder at most (`publisher/repository/file`, with room to spare).
const FOLDER_DEPTH: usize = 5;

fn has_size(path: &Path, size: u64) -> bool {
    path.metadata().is_ok_and(|m| m.is_file() && m.len() == size)
}

/// A complete copy of `model` in one of the stores: where the app's download goes, then the two stores that
/// name a file after its SHA-256 (so the name alone says it is the same file), then the folders that keep a
/// file under its own name.
fn find_model_file(model: &ModelFile, stores: &Stores) -> Option<PathBuf> {
    let repo_dir = format!("models--{}", model.repo.replace('/', "--"));
    let named = [
        stores.app.join(model.file),
        stores.hugging_face.join(repo_dir).join("blobs").join(model.sha256),
        stores.ollama.join("blobs").join(format!("sha256-{}", model.sha256)),
    ];
    if let Some(path) = named.into_iter().find(|p| has_size(p, model.size)) {
        return Some(path);
    }
    let mut found = None;
    for (_, folder) in &stores.folders {
        each_file(folder, FOLDER_DEPTH, &mut |path, size| {
            if found.is_none() && size == model.size && path.file_name().is_some_and(|name| name == model.file) {
                found = Some(path.to_path_buf());
            }
        });
    }
    found
}

/// Where the recommended model was last found, and when it was last looked for: the window asks every frame,
/// and the folders are not walked again for each one.
static RECOMMENDED_AT: Mutex<(Option<PathBuf>, Option<Instant>)> = Mutex::new((None, None));

fn recommended_model() -> Option<PathBuf> {
    let mut known = RECOMMENDED_AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if known.0.as_deref().is_some_and(|path| has_size(path, RECOMMENDED.size)) {
        return known.0.clone();
    }
    if known.1.is_some_and(|searched| searched.elapsed() < Duration::from_secs(2)) {
        return None;
    }
    *known = (find_model_file(&RECOMMENDED, &stores()), Some(Instant::now()));
    known.0.clone()
}

/// The model file the engine would load: the file the user chose, else the recommended model wherever a
/// complete copy of it is found.
pub fn model(chosen: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = chosen {
        return if path.is_file() { Ok(path.to_path_buf()) } else { Err(format!("選んだモデルが無い: {}", path.display())) };
    }
    recommended_model().ok_or_else(|| format!("内蔵エンジンのモデル（{}）がまだ無い", RECOMMENDED.file))
}

/// A model file found on this Mac that the user may choose instead of the recommended one.
#[derive(Clone, PartialEq, Debug)]
pub struct Found {
    /// The tool that holds it and the name it has there.
    pub label: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Smaller GGUF files are not translation models: projectors, draft models, embeddings.
const SMALLEST_MODEL: u64 = 1_000_000_000;

fn is_gguf(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut magic)).is_ok() && &magic == b"GGUF"
}

/// The GGUF models Ollama holds, under the names Ollama gives them. Its MLX models are not GGUF files and are
/// left out. A model from Ollama's own library may still fail to load: Ollama writes some of them in a form
/// only Ollama reads.
fn ollama_models(ollama: &Path) -> Vec<Found> {
    let manifests = ollama.join("manifests");
    let mut found = Vec::new();
    // manifests/<host>/<namespace>/<model>/<tag>
    each_file(&manifests, 3, &mut |manifest, _| {
        let Some(parts) = manifest.strip_prefix(&manifests).ok().map(|rest| {
            rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>()
        }) else {
            return;
        };
        let [host, namespace, name, tag] = parts.as_slice() else {
            return;
        };
        let name = if host == "registry.ollama.ai" && namespace == "library" {
            format!("{name}:{tag}")
        } else {
            format!("{host}/{namespace}/{name}:{tag}")
        };
        let Some(layers) = std::fs::read(manifest).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            return;
        };
        for layer in layers["layers"].as_array().into_iter().flatten() {
            let (Some("application/vnd.ollama.image.model"), Some(digest)) =
                (layer["mediaType"].as_str(), layer["digest"].as_str())
            else {
                continue;
            };
            let path = ollama.join("blobs").join(digest.replace(':', "-"));
            if let Ok(meta) = path.metadata() {
                if meta.len() >= SMALLEST_MODEL && is_gguf(&path) {
                    found.push(Found { label: format!("Ollama: {name}"), path, size: meta.len() });
                }
            }
        }
    });
    found
}

/// The GGUF models on this Mac other than the recommended one, for the user to choose from: what Ollama holds,
/// and the `.gguf` files in the other tools' folders, the Hugging Face cache and the app's own folder.
fn other_models(model: &ModelFile, stores: &Stores) -> Vec<Found> {
    let mut found = ollama_models(&stores.ollama);
    let folders = stores
        .folders
        .iter()
        .map(|(tool, folder)| (*tool, folder.as_path()))
        .chain([("Hugging Face", stores.hugging_face.as_path()), ("Live Subtitle", stores.app.as_path())]);
    for (tool, folder) in folders {
        each_file(folder, FOLDER_DEPTH, &mut |path, size| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if size < SMALLEST_MODEL || !name.ends_with(".gguf") || name.contains("mmproj") {
                return;
            }
            // A Hugging Face path is models--<publisher>--<repository>/snapshots/<revision>/<file>.
            let repository = path
                .strip_prefix(folder)
                .ok()
                .and_then(|rest| rest.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .and_then(|first| first.strip_prefix("models--").map(|repo| repo.replace("--", "/")));
            let label = match repository {
                Some(repository) => format!("{tool}: {repository}/{name}"),
                None => format!("{tool}: {name}"),
            };
            found.push(Found { label, path: path.to_path_buf(), size });
        });
    }
    // The recommended model is offered on its own, and one file reached by two names is listed once.
    found.retain(|f| !(f.size == model.size && f.path.file_name().is_some_and(|name| name == model.file)));
    found.retain(|f| f.path != stores.ollama.join("blobs").join(format!("sha256-{}", model.sha256)));
    let mut seen = Vec::new();
    found.retain(|f| {
        let real = f.path.canonicalize().unwrap_or_else(|_| f.path.clone());
        !seen.contains(&real) && {
            seen.push(real);
            true
        }
    });
    found.sort_by(|a, b| a.label.cmp(&b.label));
    found
}

/// Looks through the stores for models to offer beside the recommended one. It reads folders, so the window
/// runs it off its own thread.
pub fn discover() -> Vec<Found> {
    other_models(&RECOMMENDED, &stores())
}

/// The server binary: beside the app's own executable, where `scripts/bundle.sh` puts it.
fn server_binary() -> Result<PathBuf, String> {
    let path = match std::env::var_os("LIVE_SUBTITLE_LLAMA_SERVER") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("llama-server")))
            .ok_or("アプリの場所が分からない")?,
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("llama-server が無い: {}（scripts/bundle.sh で作ったアプリに入っている）", path.display()))
    }
}

struct Server {
    child: Child,
    port: u16,
    model: PathBuf,
    /// Whether the model has finished loading.
    ready: bool,
}

/// The running server, if any. Held only for a moment, so stopping never waits for a model to load.
static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// Held from the start of a server until its model is loaded (about 7 s), so there is one start at a time and
/// lines heard meanwhile wait for the load instead of being given up after `QUEUE_LIMIT`.
static STARTING: Mutex<()> = Mutex::new(());
/// The server is asked for one line at a time. Without this it would queue every line, however late.
static TURN: Mutex<()> = Mutex::new(());
/// Set while the app is shutting down: no server may be started any more.
static CLOSED: AtomicBool = AtomicBool::new(false);
/// Whether a line to translate may start the server. A warm-up turns it on; `stop` and a start that failed
/// turn it off, so a line that was already on its way cannot bring a stopped server (and its 14 GB) back, and
/// a model that does not load is not tried again for every line heard.
static ARMED: AtomicBool = AtomicBool::new(false);

/// A model that has not loaded by then is not going to (a cold 14 GB read takes well under a minute).
const START_LIMIT: Duration = Duration::from_secs(300);

fn server() -> MutexGuard<'static, Option<Server>> {
    SERVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pid_file() -> PathBuf {
    data_dir().join("llama-server.pid")
}

fn log_file() -> PathBuf {
    data_dir().join("llama-server.log")
}

fn end(mut server: Server) {
    let _ = server.child.kill();
    let _ = server.child.wait();
    let _ = std::fs::remove_file(pid_file());
}

/// Frees the model's memory by ending the server. The next line to translate starts it again.
pub fn release() {
    let running = server().take();
    if let Some(running) = running {
        end(running);
    }
}

/// Stops the engine: the server ends and stays down until the next warm-up.
pub fn stop() {
    ARMED.store(false, Ordering::SeqCst);
    release();
}

/// Stops the server for good: the app is shutting down.
pub fn close() {
    CLOSED.store(true, Ordering::SeqCst);
    stop();
}

/// Kills a server left behind by an app that was force-quit or crashed: the process recorded in the pid file,
/// when it still runs the same binary and has lost its parent.
pub fn reap_stale() {
    let Ok(record) = std::fs::read_to_string(pid_file()) else {
        return;
    };
    let _ = std::fs::remove_file(pid_file());
    let mut lines = record.lines();
    let (Some(pid), Some(binary)) = (lines.next().and_then(|l| l.trim().parse::<u32>().ok()), lines.next()) else {
        return;
    };
    let Ok(ps) = Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "ppid=", "-o", "comm="]).output() else {
        return;
    };
    let line = String::from_utf8_lossy(&ps.stdout);
    let Some((parent, command)) = line.trim().split_once(char::is_whitespace) else {
        return;
    };
    if parent.trim() == "1" && command.trim() == binary {
        let _ = Command::new("/bin/kill").args(["-KILL", &pid.to_string()]).status();
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into()
}

/// Why the server did not start, from what it wrote: its first error line (the later ones only repeat that the
/// model was not loaded), else its last line.
fn failure_reason() -> String {
    reason_in(&std::fs::read_to_string(log_file()).unwrap_or_default())
}

fn reason_in(log: &str) -> String {
    let lines = || log.lines().filter(|line| !line.trim().is_empty());
    match lines().find_map(|line| line.split_once(" E ").map(|(_, error)| error)) {
        Some(error) => error.to_string(),
        None => lines().last().unwrap_or("理由は記録されていない").to_string(),
    }
}

const STOPPED: &str = "内蔵エンジンは止まっている";

/// Returns the port of a server that has `model` loaded, starting one (and stopping one with another model
/// first, so two models are never in memory together) when needed. Only a warm-up (`arm`) may start the
/// engine after it was stopped.
fn ensure_started(model: &Path, arm: bool) -> Result<u16, String> {
    if arm {
        ARMED.store(true, Ordering::SeqCst);
    }
    let _starting = STARTING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Asked after the wait: a stop that came while another start held the lock must win.
    if !ARMED.load(Ordering::SeqCst) {
        return Err(STOPPED.into());
    }
    let started = start(model);
    // A start that was stopped is already disarmed; disarming it here could undo a warm-up that came after.
    if started.as_ref().is_err_and(|error| error != STOPPED) {
        ARMED.store(false, Ordering::SeqCst);
    }
    started
}

fn start(model: &Path) -> Result<u16, String> {
    let stale = {
        let mut current = server();
        if let Some(s) = current.as_mut() {
            if s.ready && s.model == model && matches!(s.child.try_wait(), Ok(None)) {
                return Ok(s.port);
            }
        }
        current.take()
    };
    if let Some(stale) = stale {
        end(stale);
    }
    let binary = server_binary()?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map_err(|e| format!("空いているポートが無い: {e}"))?
        .port();
    std::fs::create_dir_all(data_dir()).map_err(|e| e.to_string())?;
    // The server's output goes to a file: an unread pipe would fill up and stall it.
    let log = std::fs::File::create(log_file()).map_err(|e| e.to_string())?;
    let errors = log.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new(&binary)
        .arg("-m")
        .arg(model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        // Everything on the GPU, one request at a time, room for one subtitle and its context, no thinking,
        // no browser UI.
        .args(["-ngl", "999", "-np", "1", "-c", "4096", "--reasoning-budget", "0", "--no-ui"])
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors)
        .spawn()
        .map_err(|e| format!("llama-server を起動できない: {e}"))?;
    let pid = child.id();
    {
        let mut current = server();
        let started = Server { child, port, model: model.to_path_buf(), ready: false };
        if CLOSED.load(Ordering::SeqCst) {
            end(started);
            return Err("終了処理中".into());
        }
        let _ = std::fs::write(pid_file(), format!("{pid}\n{}\n", binary.display()));
        *current = Some(started);
    }
    let health = agent(Duration::from_secs(2));
    let deadline = Instant::now() + START_LIMIT;
    loop {
        {
            let mut current = server();
            match current.as_mut() {
                Some(s) if s.child.id() == pid => {
                    if let Ok(Some(status)) = s.child.try_wait() {
                        *current = None;
                        let _ = std::fs::remove_file(pid_file());
                        return Err(format!("llama-server が起動できなかった（{status}）: {}", failure_reason()));
                    }
                }
                _ => return Err(STOPPED.into()),
            }
        }
        // 503 while the model loads, 200 once it can answer.
        if health.get(&format!("http://127.0.0.1:{port}/health")).call().is_ok() {
            return match server().as_mut() {
                Some(s) if s.child.id() == pid => {
                    s.ready = true;
                    Ok(port)
                }
                _ => Err(STOPPED.into()),
            };
        }
        if Instant::now() > deadline {
            release();
            return Err(format!("モデルの読み込みが {} 秒で終わらなかった", START_LIMIT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn request(port: u16, system: &str, text: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "temperature": 0,
        "max_tokens": 400,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": text },
        ],
    });
    let mut response = agent(Duration::from_secs(90))
        .post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .send_json(&body)
        .map_err(|e| format!("内蔵エンジンが応答しない: {e}"))?;
    let reply: serde_json::Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
    reply["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("内蔵エンジンの応答が不正: {reply}"))
}

/// Translates one line, giving up when the lines ahead of it take longer than `QUEUE_LIMIT`.
pub fn translate(chosen: Option<&Path>, system: &str, text: &str) -> Result<String, String> {
    let port = ensure_started(&model(chosen)?, false)?;
    let _turn = lock_within(&TURN, QUEUE_LIMIT)?;
    request(port, system, text)
}

/// Starts the server and runs one line through it, so the first subtitle does not wait for the model to load.
pub fn warm_up(chosen: Option<&Path>, system: &str) -> Result<(), String> {
    let port = ensure_started(&model(chosen)?, true)?;
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    request(port, system, "Hello.").map(|_| ())
}

/// A download of the recommended model, running on its own thread.
pub struct Download {
    received: Arc<AtomicU64>,
    verifying: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<PathBuf, String>>,
}

impl Download {
    /// Bytes on disk so far, including what an earlier, interrupted download left.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Whether everything has arrived and the checksum is being computed.
    pub fn verifying(&self) -> bool {
        self.verifying.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The outcome once the download has ended: where the model is, or why there is none.
    pub fn finished(&self) -> Option<Result<PathBuf, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("ダウンロードが途中で止まった".into())),
        }
    }
}

/// True while a download thread is alive, so a second one never writes to the same file.
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

/// Downloads the recommended model into the app's folder, continuing a partial download left there.
pub fn download_recommended(repaint: impl Fn() + Send + 'static) -> Result<Download, String> {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("前のダウンロードがまだ終わっていない".into());
    }
    let received = Arc::new(AtomicU64::new(0));
    let verifying = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, result) = std::sync::mpsc::channel();
    let progress = Progress { received: received.clone(), verifying: verifying.clone(), cancel: cancel.clone() };
    std::thread::spawn(move || {
        let model = &RECOMMENDED;
        let destination = data_dir().join(model.file);
        let outcome = fetch(&recommended_url(), &destination, model.size, model.sha256, &progress, &repaint);
        DOWNLOADING.store(false, Ordering::SeqCst);
        let _ = tx.send(outcome.map(|()| destination));
        repaint();
    });
    Ok(Download { received, verifying, cancel, result })
}

/// The recommended model at the pinned revision, so the bytes are the ones the checksum was taken from.
fn recommended_url() -> String {
    format!("https://huggingface.co/{}/resolve/{}/{}", RECOMMENDED.repo, RECOMMENDED.revision, RECOMMENDED.file)
}

struct Progress {
    received: Arc<AtomicU64>,
    verifying: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

const CANCELLED: &str = "ダウンロードを中止した（もう一度押すと続きから再開する）";

/// Downloads `url` to `destination` through a `.part` file beside it, continuing a `.part` file that is
/// already there, and puts the file in place only when its size and SHA-256 are the expected ones.
fn fetch(
    url: &str,
    destination: &Path,
    size: u64,
    sha256: &str,
    progress: &Progress,
    repaint: &dyn Fn(),
) -> Result<(), String> {
    let part = PathBuf::from(format!("{}.part", destination.display()));
    if let Some(dir) = destination.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut have = part.metadata().map(|m| m.len()).unwrap_or(0);
    if have > size {
        have = 0;
    }
    progress.received.store(have, Ordering::Relaxed);
    if have < size {
        // No overall time limit: the transfer takes as long as the line needs.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(None)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut request = agent.get(url);
        if have > 0 {
            request = request.header("Range", &format!("bytes={have}-"));
        }
        let response = request.call().map_err(|e| format!("ダウンロードできない: {e}"))?;
        let mut file = match response.status().as_u16() {
            206 => std::fs::OpenOptions::new().append(true).open(&part),
            // The server sent the whole file (it ignored the range, or there was nothing to continue).
            200 => {
                have = 0;
                std::fs::File::create(&part)
            }
            status => return Err(format!("ダウンロードできない: HTTP {status}")),
        }
        .map_err(|e| e.to_string())?;
        let mut body = response.into_body().into_reader();
        let mut buffer = vec![0u8; 1 << 20];
        let mut shown = Instant::now();
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let n = body.read(&mut buffer).map_err(|e| format!("ダウンロードが途切れた（もう一度押すと続きから再開する）: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n]).map_err(|e| format!("モデルを書き込めない: {e}"))?;
            have += n as u64;
            progress.received.store(have, Ordering::Relaxed);
            if shown.elapsed() > Duration::from_millis(250) {
                shown = Instant::now();
                repaint();
            }
        }
        file.flush().map_err(|e| e.to_string())?;
    }
    if have != size {
        return Err(format!(
            "ダウンロードが途中で終わった（{have} / {size} バイト。もう一度押すと続きから再開する）"
        ));
    }
    progress.verifying.store(true, Ordering::Relaxed);
    repaint();
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(&part).map_err(|e| e.to_string())?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if digest != sha256 {
        let _ = std::fs::remove_file(&part);
        return Err("ダウンロードしたモデルのチェックサムが合わない（壊れているので消した。もう一度ダウンロードする）".into());
    }
    std::fs::rename(&part, destination).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A file server that honours `Range: bytes=N-` and records the Range header of every request.
    fn serve_file(content: &'static [u8]) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.gguf", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let seen = ranges.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut data = Vec::new();
                let mut buffer = [0u8; 4096];
                while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(n) if n > 0 => data.extend_from_slice(&buffer[..n]),
                        _ => break,
                    }
                }
                let head = String::from_utf8_lossy(&data).to_lowercase();
                let range = head.lines().find_map(|l| l.strip_prefix("range:").map(|v| v.trim().to_string()));
                let start = range
                    .as_deref()
                    .and_then(|r| r.strip_prefix("bytes="))
                    .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
                seen.lock().unwrap().push(range.clone());
                let (status, body) = match start {
                    Some(start) => ("206 Partial Content", &content[start..]),
                    None => ("200 OK", content),
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            }
        });
        (url, ranges)
    }

    fn sha256_of(content: &[u8]) -> String {
        Sha256::digest(content).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn progress() -> Progress {
        Progress { received: Arc::default(), verifying: Arc::default(), cancel: Arc::default() }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("live-subtitle-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A small stand-in for the recommended model, and the stores of a Mac that has every tool.
    const MODEL: ModelFile = ModelFile {
        file: "model-q4.gguf",
        repo: "publisher/model-gguf",
        revision: "main",
        size: 12,
        sha256: "00aa",
    };

    fn stores_in(dir: &Path) -> Stores {
        Stores {
            app: dir.join("app"),
            hugging_face: dir.join("hub"),
            ollama: dir.join("ollama"),
            folders: vec![("LM Studio", dir.join("lmstudio"))],
        }
    }

    fn put(path: &Path, content: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn the_recommended_model_is_found_wherever_a_tool_keeps_it() {
        let dir = scratch("find");
        let stores = stores_in(&dir);
        assert_eq!(find_model_file(&MODEL, &stores), None);
        // A file of the right name but the wrong size is an unfinished download.
        put(&stores.app.join("model-q4.gguf"), b"GGUF");
        assert_eq!(find_model_file(&MODEL, &stores), None);
        for place in [
            dir.join("lmstudio/publisher/model-gguf/model-q4.gguf"),
            dir.join("ollama/blobs/sha256-00aa"),
            dir.join("hub/models--publisher--model-gguf/blobs/00aa"),
        ] {
            put(&place, b"GGUF12345678");
            assert_eq!(find_model_file(&MODEL, &stores), Some(place.clone()));
            std::fs::remove_file(place).unwrap();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn other_models_are_listed_under_the_name_their_tool_gives_them() {
        let dir = scratch("discover");
        let stores = stores_in(&dir);
        let model = ModelFile { size: SMALLEST_MODEL, ..MODEL };
        let big = |path: PathBuf| {
            put(&path, b"GGUF");
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(SMALLEST_MODEL).unwrap();
            path
        };
        // Ollama: a GGUF model from its library, one pulled from Hugging Face, and an MLX model (tensor layers).
        let manifest = |digest: &str, media: &str| format!(r#"{{"layers":[{{"mediaType":"{media}","digest":"sha256:{digest}"}}]}}"#);
        let gguf = "application/vnd.ollama.image.model";
        put(&dir.join("ollama/manifests/registry.ollama.ai/library/gemma/26b"), manifest("11", gguf).as_bytes());
        put(&dir.join("ollama/manifests/hf.co/someone/model-GGUF/Q6_K"), manifest("22", gguf).as_bytes());
        put(&dir.join("ollama/manifests/registry.ollama.ai/library/gemma/26b-mlx"), manifest("33", "application/vnd.ollama.image.tensor").as_bytes());
        let library = big(dir.join("ollama/blobs/sha256-11"));
        let pulled = big(dir.join("ollama/blobs/sha256-22"));
        big(dir.join("ollama/blobs/sha256-33"));
        // The recommended model itself, in Ollama and in LM Studio, is not offered a second time.
        big(dir.join("ollama/blobs/sha256-00aa"));
        put(&dir.join("ollama/manifests/hf.co/publisher/model-gguf/latest"), manifest("00aa", gguf).as_bytes());
        big(dir.join("lmstudio/publisher/model-gguf/model-q4.gguf"));
        // Another model in LM Studio, with its projector; one in the Hugging Face cache, reached through a link.
        let studio = big(dir.join("lmstudio/other/repo/other-q8.gguf"));
        big(dir.join("lmstudio/other/repo/mmproj-other.gguf"));
        let blob = big(dir.join("hub/models--org--name-GGUF/blobs/44"));
        let snapshot = dir.join("hub/models--org--name-GGUF/snapshots/abc/name.gguf");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&blob, &snapshot).unwrap();

        let found: Vec<(String, PathBuf)> = other_models(&model, &stores).into_iter().map(|f| (f.label, f.path)).collect();
        assert_eq!(
            found,
            [
                ("Hugging Face: org/name-GGUF/name.gguf".to_string(), snapshot),
                ("LM Studio: other-q8.gguf".to_string(), studio),
                ("Ollama: gemma:26b".to_string(), library),
                ("Ollama: hf.co/someone/model-GGUF:Q6_K".to_string(), pulled),
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What the server wrote when it was given a model in Ollama's own format (`gpt-oss:20b`).
    #[test]
    fn the_reason_a_model_did_not_load_is_the_first_error_the_server_wrote() {
        let log = "0.00.000.070 I srv  llama_server: initializing ...\n\
0.05.371.256 E llama_model_load: error loading model: unknown model architecture: 'gptoss'\n\
0.05.371.448 E llama_model_load_from_file_impl: failed to load model\n\
0.05.434.559 E srv  server_run_i: exiting due to model loading error\n";
        assert_eq!(reason_in(log), "llama_model_load: error loading model: unknown model architecture: 'gptoss'");
        assert_eq!(reason_in("dyld: Library not loaded\n\n"), "dyld: Library not loaded");
        assert_eq!(reason_in(""), "理由は記録されていない");
    }

    #[test]
    fn a_partial_download_is_continued_from_where_it_stopped() {
        const CONTENT: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let (url, ranges) = serve_file(CONTENT);
        let dir = scratch("resume");
        let destination = dir.join("model.gguf");
        std::fs::write(dir.join("model.gguf.part"), &CONTENT[..10]).unwrap();
        let progress = progress();
        fetch(&url, &destination, CONTENT.len() as u64, &sha256_of(CONTENT), &progress, &|| {}).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), CONTENT);
        assert_eq!(*ranges.lock().unwrap(), [Some("bytes=10-".to_string())]);
        assert_eq!(progress.received.load(Ordering::Relaxed), CONTENT.len() as u64);
        assert!(!dir.join("model.gguf.part").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_download_with_the_wrong_checksum_is_not_put_in_place() {
        const CONTENT: &[u8] = b"not the model that was asked for";
        let (url, _) = serve_file(CONTENT);
        let dir = scratch("checksum");
        let destination = dir.join("model.gguf");
        let error = fetch(&url, &destination, CONTENT.len() as u64, &sha256_of(b"the model"), &progress(), &|| {}).unwrap_err();
        assert!(error.contains("チェックサム"), "{error}");
        assert!(!destination.exists() && !dir.join("model.gguf.part").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The main route against the real server and the real model: start, translate, stop, and clean up after
    /// an app that died. Run it with `LIVE_SUBTITLE_LLAMA_SERVER=<llama-server> cargo test -- --ignored real_server`.
    #[test]
    #[ignore = "needs llama-server and the 14 GB model"]
    fn the_real_server_translates_stops_and_orphans_are_reaped() {
        let model = model(None).unwrap();
        let system = crate::translate::system_prompt("ja", "en", &[]);
        warm_up(None, &system).unwrap();
        let pid = server().as_ref().unwrap().child.id();
        let recorded = std::fs::read_to_string(pid_file()).unwrap();
        assert!(recorded.starts_with(&format!("{pid}\n")), "{recorded}");
        let japanese = translate(None, &system, "Okay so today we're making a really simple tomato sauce.").unwrap();
        assert!(japanese.contains("トマトソース") && japanese.ends_with('。'), "{japanese}");
        // The same model again reuses the running server.
        assert_eq!(ensure_started(&model, false).unwrap(), server().as_ref().unwrap().port);
        assert_eq!(server().as_ref().unwrap().child.id(), pid);
        stop();
        assert!(server().is_none() && !pid_file().exists());
        let alive = Command::new("/bin/kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().unwrap();
        assert!(!alive.success(), "the server is still running");

        // A stop while the model loads wins: the warm-up gives up, and a line that was waiting for the load
        // does not start the server again.
        let loading = std::thread::spawn(move || warm_up(None, &crate::translate::system_prompt("ja", "en", &[])));
        std::thread::sleep(Duration::from_millis(150));
        let waiting = std::thread::spawn(|| translate(None, "Translate into Japanese.", "Hello."));
        std::thread::sleep(Duration::from_millis(150));
        let pid = server().as_ref().expect("the server was started").child.id();
        stop();
        assert_eq!(loading.join().unwrap(), Err(STOPPED.to_string()));
        assert_eq!(waiting.join().unwrap(), Err(STOPPED.to_string()));
        assert!(server().is_none());
        let alive = Command::new("/bin/kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().unwrap();
        assert!(!alive.success(), "the server came back after the stop");
        // Freeing memory, unlike a stop, lets the next line load the model again.
        warm_up(None, &system).unwrap();
        release();
        assert!(server().is_none());
        assert!(translate(None, &system, "Good morning.").is_ok() && server().is_some());
        stop();

        // A server whose app is gone (its parent is launchd) and that the pid file names is killed at launch.
        let binary = server_binary().unwrap();
        let orphan = Command::new("/bin/sh")
            .args(["-c", r#""$1" -m "$2" --port 18111 >/dev/null 2>&1 & echo $!"#, "sh"])
            .arg(&binary)
            .arg(&model)
            .output()
            .unwrap();
        let orphan = String::from_utf8_lossy(&orphan.stdout).trim().to_string();
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(pid_file(), format!("{orphan}\n{}\n", binary.display())).unwrap();
        reap_stale();
        std::thread::sleep(Duration::from_millis(500));
        let alive = Command::new("/bin/kill").args(["-0", &orphan]).stderr(Stdio::null()).status().unwrap();
        assert!(!alive.success() && !pid_file().exists(), "the orphaned server {orphan} is still running");
    }

    /// The real host continues a download: a copy of the model that lacks its last kilobyte is completed and
    /// passes the checksum. Run it with `cargo test -- --ignored real_host` (it fetches one kilobyte).
    #[test]
    #[ignore = "needs the 14 GB model and the network"]
    fn the_real_host_continues_a_download() {
        let dir = scratch("real-host");
        let destination = dir.join(RECOMMENDED.file);
        let part = dir.join(format!("{}.part", RECOMMENDED.file));
        std::fs::copy(model(None).unwrap(), &part).unwrap();
        std::fs::OpenOptions::new().write(true).open(&part).unwrap().set_len(RECOMMENDED.size - 1024).unwrap();
        let progress = progress();
        fetch(&recommended_url(), &destination, RECOMMENDED.size, RECOMMENDED.sha256, &progress, &|| {}).unwrap();
        assert_eq!(destination.metadata().unwrap().len(), RECOMMENDED.size);
        assert!(progress.verifying.load(Ordering::Relaxed) && !part.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
