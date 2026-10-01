# Live Subtitle

English | [日本語](README.ja.md) · [MIT License](LICENSE)

A macOS GUI tool (Rust, eframe/egui) that listens to everything your Mac plays (YouTube, X, meetings — any app) and shows it as Japanese subtitles.

```
system audio ─ ScreenCaptureKit ─ auto gain ─ utterance ─ whisper.cpp (Metal) ─ translation ─ subtitles
                 16 kHz mono        (AGC)      splitting    language auto-detect   Ollama / Claude / Codex
                                              (relative
                                               threshold)
```

- The original text appears immediately; the Japanese translation is filled in as soon as it is ready, so a slow translation never stalls the subtitles.
- Japanese speech is shown as is, without translation.
- An input level meter sits at the top of the window (green; yellow when loud; gray with "無音" when silent). It shows the level after automatic gain control; hover the bar to see the applied gain (dB).
- Press "会話履歴を保存" (save conversation) to write the current subtitles (original, translation, time) to a timestamped text file and reveal it in Finder. Nothing is saved unless you press it. The default folder is the Desktop (file name `Live Subtitle <date> <time>.txt`); "保存先を選ぶ…" (choose folder) lets you pick any folder, and the choice is remembered ("デスクトップに戻す" returns to the default). macOS may ask for access to that folder the first time.
- While models load (whisper, Ollama), the window shows a "…モデル読み込み中" (loading model) message.
- The window stays on top by default (toggle available).
- "帯にする" (make a band) switches to a compact subtitle-only view, like a TV caption: a borderless, translucent strip that opens **centered on where the normal window was, and as wide as it was** (to match a video's width, resize the normal window first). The previous position is deliberately not reused, because a band that reappears at an old position gets lost. For the first 10 seconds its frame blinks yellow so you can find it. Drag it anywhere and drag its edges to resize it (the text scales with the band's height; only the height is remembered). It fades out when nothing is being said and floats over full-screen video as well. **Esc** (or the "元に戻す" restore button that appears on hover) returns to the normal window at its original size and position. Esc works only while the band has focus (click the band to focus it).

## What to prepare in advance

**To build**

- An Apple Silicon Mac (macOS 13 or later)
- Xcode (or the Command Line Tools) — Swift is needed to build the ScreenCaptureKit bridge
- Rust and cmake (`brew install cmake`; used to build whisper.cpp)

**Speech recognition (required): one whisper model (about 574 MB)**

```bash
mkdir -p ~/"Library/Application Support/LiveSubtitle"
curl -L -o ~/"Library/Application Support/LiveSubtitle/ggml-large-v3-turbo-q5_0.bin" \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin
```

To keep the model elsewhere, set `LIVE_SUBTITLE_MODEL` to its path.

**Translation backend (at least one; not needed if you use "翻訳しない" / no translation)**

- **Ollama (recommended; free and offline)**: install and start Ollama, then pull a model.
  ```bash
  ollama pull gemma4:26b-mlx
  ```
  Choose a model your Ollama can pull (the window's menu lists the installed ones). Large models use a lot of memory — see "Recommended translation model" and "Releasing Ollama's memory" below.
- **Claude**: the `claude` command of Claude Code (for example in `~/.local/bin`; set `LIVE_SUBTITLE_CLAUDE` for another location). Sign-in is required (the button in the window can start it).
- **Codex**: the `codex` command (set `LIVE_SUBTITLE_CODEX` for another location). Signing in with a ChatGPT account is required (the button in the window can start it).

If Claude or Codex is not signed in, the "サインイン" (sign in) button in the window starts the official browser OAuth flow. The app never handles credentials: it only checks the status and starts the official procedure (`claude auth` for Claude; the App Server's `account/*` methods for Codex).

**Signing identity (recommended)**: use a fixed code-signing identity so the Screen Recording permission survives rebuilds (see "Build and run" below).

## Recommended translation model

**Ollama's `gemma4:26b-mlx`** feels comfortable in use (the author's impression). Measured values:

| Item | Value |
|---|---|
| Translation per sentence | about 0.65–2 s (after the first; only the first load takes about 13 s) |
| Disk | about 18 GB |
| Memory (while loaded) | about 25.7 GB |

`gpt-oss:20b` (13 GB on disk, about 13.7 GB in memory) and `qwen3.8:27b-mlx` (18 GB on disk, about 20.0 GB in memory) were also confirmed to be selectable from the window's menu (their speed was not measured). Loading several models at once can exceed 50 GB.

## Verified environment

macOS 27.0.1 / Apple M1 Max (64 GB) / Ollama 0.35.0 / Claude Code 2.1.286 / codex-cli 0.159.0 / Rust 1.95.0 / cmake 4.4.3 / Swift 6.4

## Build and run

```bash
CODESIGN_IDENTITY="<signing identity>" scripts/bundle.sh   # default output: ~/Applications/Live Subtitle.app
```

The build happens in a temporary directory (codesign fails under iCloud-synced folders). Without `CODESIGN_IDENTITY` the app gets an ad-hoc signature and **the Screen Recording permission is lost on every rebuild**, so use a fixed identity.

For the permissions macOS asks for, see "macOS permissions" below.

## macOS permissions

| Permission | Needed? | When | What it is |
|---|---|---|---|
| **Screen & System Audio Recording** | **Required** | the first time you press "開始" (start) | needed to capture system audio. The app does not use any screen image — it takes audio only |
| Folder access (the Desktop, or the folder you chose) | only when saving a conversation | the first save | allow it when macOS asks |
| Microphone | not needed | — | the app does not use the microphone (it captures what your Mac plays) |
| Accessibility / Input Monitoring | not needed | — | Esc is read only while the band has focus; there is no global key monitoring |

If macOS asks for anything else (folder access and the like), just read the prompt and allow it as needed.

**Granting Screen & System Audio Recording**

1. Launch `Live Subtitle.app` and press "開始". Without the permission a red message "画面収録の許可が必要: …" (screen recording permission required) appears (macOS may also show a dialog).
2. Open System Settings → Privacy & Security → **Screen & System Audio Recording** and turn on `Live Subtitle`. If it is not listed, press "+" and add `~/Applications/Live Subtitle.app` (in the file picker, press `Cmd+Shift+G` and paste the path).
3. Press "開始" again. If it is still denied, quit the app and launch it again.

**Easy-to-miss points**

- The permission is tied to the app's **code signature**. With an ad-hoc signature it is lost on every rebuild; use a fixed signing identity (see "Build and run").
- After the signature changed, access can stay denied even though the toggle is on (a stale entry for the old signature remains). Remove that entry with "−" and register the app again.
- Keep the app you authorize in a place that is easy to find in a file picker, such as `~/Applications`, not a hidden location like `/tmp`.

## When something goes wrong (things actually hit during development)

| Symptom | Cause and fix |
|---|---|
| The red "画面収録の許可が必要" message appears | Follow "Granting Screen & System Audio Recording" above. If it appears although the toggle is on, remove the entry with "−" and register it again |
| Sound is playing but the meter stays at "無音" (silent) / no subtitles | Check the permission first. Also remember that the app's own sound is excluded from capture, and that a quiet source is lifted by the automatic gain control |
| Subtitles lag | It depends on the backend and model: Ollama and Claude take about 1–2 s, Codex about 2–5 s. With Codex pick a lighter model such as Luna or Terra. A line that waits more than 10 s for a translator is shown untranslated |
| The first subtitle after starting is slow | The model is loading (Ollama about 13 s, Claude about 6 s, Codex about 5 s). Wait while "…モデル読み込み中" is shown |
| Your Mac uses 50 GB+ of memory | A large Ollama model is loaded. Press "メモリ解放" (it is also released automatically at start, at exit and on a model switch; what a forced kill left behind is cleared at the next launch) |
| Your Mac's sound suddenly got loud while testing | The app never changes the Mac's output volume or the signal sent to your speakers; the automatic gain control touches only the captured copy. The loudness seen during development came from full-level test sounds (`say`, `afplay`), not from the app's gain |
| You lost the band | For the first 10 seconds its frame blinks yellow. Click the band to focus it, then press Esc or the restore button to return to the normal window. Esc works only while the band has focus |
| Claude sign-in does not finish | If the browser flow does not complete by itself, run `claude auth login` in a terminal (the Claude sign-in action itself was not verified on the author's Mac) |
| `codesign` fails | It fails when the build happens inside an iCloud-synced folder such as `~/Desktop`. `scripts/bundle.sh` builds in a temporary directory, so build through the script |

## Translation backends

| Choice | Measured latency | Notes |
|---|---|---|
| Ollama (default `gemma4:26b-mlx`) | about 0.7–2 s (only the first load takes about 13 s; it is preloaded at start) | local, offline |
| Claude (a resident `claude -p`) | about 0.7–2 s depending on the model (only the first start takes about 6 s, during which "翻訳モデル読み込み中" is shown) | high quality; uses your plan's quota. Pick an explicit model version (Haiku 4.5, Sonnet 5.5, Opus 5.5, Fable 5.1) or type a model ID |
| Codex (a resident `codex app-server`) | about 2–5 s (varies a lot; only the first start takes about 5 s) | uses the ChatGPT account the local Codex is signed in to. Models come from `model/list` (what your account can use; the default is GPT-6.1-Sol), and the reasoning effort is selectable |
| No translation | — | original text only |

## How each backend is connected

The app uses no API keys. For Claude and Codex it starts the official commands already installed on your Mac as child processes (the login is whatever those commands already hold).

| Backend | Connection |
|---|---|
| **Claude** | **Starts the `claude` command (CLI).** One `claude -p` process runs in stream-json mode over stdin/stdout, and each subtitle line is one message to it. Thinking, tools, hooks, settings loading and session persistence are all turned off. The history grows with every message, so the process is replaced every 40 lines (the next one is started in the background beforehand). |
| **Codex** | **Starts `codex app-server` (the App Server)** and talks JSON-RPC over stdin/stdout. One server hosts six ephemeral threads, so lines are translated in parallel (`thread/start`, then `turn/start` per line, then it reads `item/agentMessage/delta` and `turn/completed`). Approval policy is `never` and the sandbox is `read-only`. The model list comes from `model/list`; sign-in status and start use `account/read` and `account/login/start`. |
| **Ollama** | **HTTP API** (default `http://127.0.0.1:11434`, changeable with `OLLAMA_HOST`). Translation is `POST /api/chat` (no streaming, no thinking, `keep_alive` 30 min); the model list is `/api/tags`; memory release uses `/api/ps` and `/api/generate`. |

The Claude and Codex processes are stopped when you press "停止" (stop) and when the app quits.

## Releasing Ollama's memory

A large model uses tens of GB of memory. The app frees it the same way Taceta's "release all models" does (ask `/api/ps` which models are loaded, send each one `keep_alive: 0`, and wait until they are gone), at these times. **Models that other apps loaded are included** (they are simply reloaded the next time something uses them).

- When you press the "メモリ解放" (release memory) button
- When the app starts (to clear what an abnormal exit left behind)
- When the app quits
- When you switch the Ollama model, or switch the engine away from Ollama (the new model is loaded only after the memory is free)
- When the app panics, or is terminated by SIGTERM / SIGINT / SIGHUP

If Ollama is not running, the app does not start it (nothing is considered loaded). If the app was killed without a chance to release, the models are cleared at the next launch; until then, Ollama frees them itself once the `keep_alive` (30 minutes) set on the translation requests runs out.

**Verified by hand**

- At launch: `gemma4:26b-mlx` (25.7 GB) and `qwen3.8:27b-mlx` (20.0 GB), which were loaded before launch, were both freed right after launch (the window shows "起動時: Ollama のモデルを 2 個、メモリから解放した").
- At exit: after loading `gpt-oss:20b` (13.7 GB), quitting the app freed it.
- On SIGTERM: likewise freed before the app exited.
- Unit tests against a fake Ollama server: freeing several models, nothing loaded, a model that cannot be freed (its error text is shown), and Ollama not running.
- Pressing the "メモリ解放" button and the release on model switch need UI interaction and were not checked automatically.

**Out of scope**

- The Claude and Codex child processes are not part of this feature; they are stopped separately on "停止" and at exit.
- Memory of anything other than Ollama (such as the whisper model) is not released.

## Environment variables

| Name | Purpose |
|---|---|
| `LIVE_SUBTITLE_MODEL` | path of the whisper model |
| `LIVE_SUBTITLE_CLAUDE` | path of the `claude` command |
| `LIVE_SUBTITLE_CODEX` | path of the `codex` command |
| `OLLAMA_HOST` | where Ollama listens |
| `LIVE_SUBTITLE_AUTOSTART` | when set, start listening as soon as the app starts |
| `LIVE_SUBTITLE_HISTORY_DIR` | folder for "会話履歴を保存" (a folder chosen in the window takes precedence) |
| `LIVE_SUBTITLE_AUTOBAND` | when set, start in band (caption) mode |
| `LIVE_SUBTITLE_DEBUG_LOG` | append input level, segmentation, recognition and translation timings to this path |

## Layout

- `src/capture.rs` system audio capture (ScreenCaptureKit; the app's own sound is excluded)
- `src/agc.rs` automatic gain control (fast attack, slow release envelope)
- `src/history.rs` saving the conversation to a file (only when the button is pressed)
- `src/pipeline.rs` utterance splitting (relative threshold, forced split at 10 s), whisper, translation threads
- `src/translate.rs` backend switching and Ollama
- `src/claude.rs` translation through a resident Claude CLI
- `src/codex.rs` translation through a resident Codex App Server
- `src/main.rs` GUI
- `src/app_shell_foundation.rs` shared display-settings component (managed by app-shell-foundation; do not edit)
- `examples/capture-spike.rs` an audio-capture-only smoke test

## Known limitations

- On this Mac the captured audio is quite quiet (RMS about 0.005); the cause is unknown. Automatic gain control (AGC: target about -20 dBFS, at most +36 dB, gain held during silence) lifts it before whisper. Utterance splitting uses a threshold relative to the signal before the gain.
- Music or ambient sound alone is still handed to whisper every 10 seconds. Hallucinated speech is suppressed by the no-speech probability and by dropping symbol-only results, but not perfectly.

## License

[MIT](LICENSE)
