# Live Subtitle

English | [日本語](README.ja.md) · [MIT License](LICENSE)

A macOS GUI tool (Rust, eframe/egui) that listens to everything your Mac plays (YouTube, X, meetings — any app) and shows it as subtitles in the language you choose (Japanese by default).

```
system audio ─ ScreenCaptureKit ─ auto gain ─ utterance ─ whisper.cpp (Metal) ─ translation ─ subtitles
                 16 kHz mono        (AGC)      splitting    language auto-detect   built-in / Ollama / Claude / Codex
                                              (Silero VAD)
```

- The original text appears immediately; the translation is filled in as soon as it is ready, so a slow translation never stalls the subtitles.
- Translation goes sentence by sentence. An utterance ends at a short pause or at the 10 s cap, often mid-sentence, so the unfinished end is shown in the original right away and held until the next utterance completes the sentence; then the whole sentence is translated (after a 2 s pause, or 15 s in any case, it is translated as it is). Translated fragment by fragment, the translation trails off ("そして、そこには…").
- The translator is told the text is transcribed talk, and asked to drop fillers (like, you know) and false starts, to use the natural word order of the chosen language, and never to end a line with "…".
- The language to translate into is chosen in the top row ("→ 日本語" by default): Japanese, English, Chinese (Simplified), Korean, Thai, Vietnamese, Indonesian, Spanish, French, German, Italian, Portuguese or Russian. The language being spoken is detected line by line, so any language whisper recognizes can be the source, Japanese included — for example, Japanese videos with Thai subtitles. Speech already in the chosen language is shown as is, without translation. Arabic, Hebrew and the Indic scripts are not offered: egui cannot lay them out, so their letters would show disconnected or out of order.
- Chinese, Korean and Thai text is drawn with the macOS fonts Hiragino Sans GB, Apple SD Gothic Neo and Thonburi, added after the UI font. egui does not position Thai marks the way a full text engine does, so a tone mark over a vowel can sit a little off.
- The window has three parts. The top row holds Start/Stop, "帯にする" (Caption bar), "設定" (Settings) and the target language, translation engine and model; the subtitles fill the middle; the bottom row holds "原文も表示" (Show original), "履歴を保存" (Save), "コピー" (Copy) and "クリア" (Clear). What you rarely change (refreshing the model list, freeing memory, keeping the window on top, the history folder, and theme, language, font and text size) is gathered in Settings, which takes the place of the subtitles while open. "原文も表示" (Show original) is both in the bottom row and under Subtitles in Settings.
- The window speaks Japanese or English: Settings → Appearance (表示) → Language (言語). "System" follows the macOS language (English unless it is Japanese). Error messages coming back from the translators and the audio capture stay in Japanese. The language subtitles are translated into is a separate choice in the top row.
- While running, a thin input level bar sits under the top row (green; yellow when loud; gray when silent, with "無音" (Silent) shown above it). It shows the level after automatic gain control; hover the bar to see the applied gain (dB).
- Press "履歴を保存" (Save) to write the current subtitles (original, translation, time) to a timestamped text file and reveal it in Finder. Nothing is saved unless you press it. The default folder is the Desktop (file name `Live Subtitle <date> <time>.txt`); "保存先を選ぶ…" (Choose folder…) in Settings lets you pick any folder, and the choice is remembered ("デスクトップに戻す" (Use Desktop) returns to the default). macOS may ask for access to that folder the first time.
- While models load (whisper, the built-in engine, Ollama), the window shows a "…モデル読み込み中" (loading model) message.
- The window stays on top by default ("最前面に固定" (Keep on top) in Settings turns it off).
- "帯にする" (make a band) switches to a compact subtitle-only view, like a TV caption: a borderless strip, its background opaque by default (Settings → Caption bar (帯) → Background transparency sets 0–100 %), that opens **centered on where the normal window was, and as wide as it was** (to match a video's width, resize the normal window first). The previous position is deliberately not reused, because a band that reappears at an old position gets lost. A hairline frame always marks its edges, and for the first 10 seconds that frame blinks yellow so you can find it. Drag it anywhere, and drag inside its edges (an 8 pt strip; the cursor changes there) to resize it (only the height is remembered). The text does not follow the band's size: it is the same size as the subtitles in the normal window (the text size in Settings changes both). A taller band stacks earlier subtitles, dimmer, above the newest. The band stays on screen while nothing is being said, and floats over full-screen video as well. **Esc** (or the "元に戻す" restore button that appears on hover) returns to the normal window at its original size and position. Esc works only while the band has focus (click the band to focus it).

## Let an AI agent build it for you

A coding agent such as Codex or Claude Code can build and install the app for you. Open the repository and ask it to "install Live Subtitle following `AGENTS.md`". The agent checks the prerequisites, downloads the whisper model (asking you first), picks a signing identity, builds and swaps in the app, and verifies that it starts. **Granting Screen Recording, signing in, and answering folder-access prompts is left to you** (the agent does not touch System Settings or credentials). It is also required to tell you before it stops a running app.

## What to prepare in advance

**To build**

- An Apple Silicon Mac (macOS 13 or later)
- Xcode (or the Command Line Tools) — Swift is needed to build the ScreenCaptureKit bridge
- Rust and cmake (`brew install cmake`; used to build whisper.cpp and the built-in engine's llama.cpp)

**Speech recognition (required): one whisper model (about 574 MB)**

```bash
mkdir -p ~/"Library/Application Support/LiveSubtitle"
curl -L -o ~/"Library/Application Support/LiveSubtitle/ggml-large-v3-turbo-q5_0.bin" \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin
```

To keep the model elsewhere, set `LIVE_SUBTITLE_MODEL` to its path.

**Translation backend (at least one; not needed if you use "翻訳しない" / no translation)**

- **Built-in (the default; free and offline; nothing else to install)**: translates with the llama.cpp inside the app. All it needs is one model (about 14.4 GB), which the window's "Download" button fetches. See "The built-in engine" below.
- **Ollama (free and offline)**: install and start Ollama, then pull a model.
  ```bash
  ollama pull gemma4:26b-mlx
  ```
  Choose a model your Ollama can pull (the window's menu lists the installed ones). Large models use a lot of memory — see "Recommended translation model" and "Releasing Ollama's memory" below.
- **Claude**: the `claude` command of Claude Code (for example in `~/.local/bin`; set `LIVE_SUBTITLE_CLAUDE` for another location). Sign-in is required (the button in the window can start it).
- **Codex**: the Codex **App Server**. The app starts it as `codex app-server`, using the same `codex` executable that the Codex CLI ships (set `LIVE_SUBTITLE_CODEX` for another location). It does not use `codex exec` or the interactive CLI. Signing in with a ChatGPT account is required (the button in the window can start it).

If Claude or Codex is not signed in, the "サインイン" (sign in) button in the window starts the official browser OAuth flow. The app never handles credentials: it only checks the status and starts the official procedure (`claude auth` for Claude; the App Server's `account/*` methods for Codex).

**Signing identity (recommended)**: use a fixed code-signing identity so the Screen Recording permission survives rebuilds (see "Build and run" below).

## Recommended translation model

**The built-in engine's Gemma 4 26B (the QAT build)** is the recommended choice, and it is the backend on the first launch (a backend chosen in an earlier version stays as it was set). It needs no other app. Measured values:

| Item | Value |
|---|---|
| Translation per sentence | about 0.5–0.9 s (median 0.75 s over 40 sentences; only the first load takes about 7 s) |
| Disk | about 14.4 GB |
| Memory (while loaded) | about 14 GB |

If you use Ollama, `gemma4:26b-mlx` can be chosen too. Its median on the same 40 sentences was 0.88 s, with no clear difference in quality from the built-in engine found (how they were compared is under "The built-in engine" below). Its first load takes about 13 s, it takes about 18 GB on disk and about 25.7 GB in memory (as Ollama reports it).

With Ollama, `gpt-oss:20b` (13 GB on disk, about 13.7 GB in memory) and `qwen3.8:27b-mlx` (18 GB on disk, about 20.0 GB in memory) were also confirmed to be selectable from the window's menu (their speed was not measured). Loading several models at once can exceed 50 GB.

## Verified environment

macOS 27.0.1 / Apple M1 Max (64 GB) / Ollama 0.35.0 / Claude Code 2.1.286 / Codex App Server 0.159.0 / Rust 1.95.0 / cmake 4.4.3 / Swift 6.4

The built-in engine was verified with macOS 27.2.1 / cmake 4.5.0 / llama.cpp b12503.

## Build and run

```bash
CODESIGN_IDENTITY="<signing identity>" scripts/bundle.sh   # default output: ~/Applications/Live Subtitle.app
```

The build happens in a temporary directory (codesign fails under iCloud-synced folders). The built-in engine's `llama-server` is built the first time only, from llama.cpp's source at a pinned release fetched from GitHub (a few minutes); the result is kept in `~/Library/Caches/LiveSubtitle/` and reused afterwards. Without `CODESIGN_IDENTITY` the app gets an ad-hoc signature and **the Screen Recording permission is lost on every rebuild**, so use a fixed identity.

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
| Subtitles lag | It depends on the backend and model: the built-in engine takes about 0.5–1 s, Ollama and Claude about 1–2 s, Codex about 2–5 s. With Codex pick a lighter model such as Luna or Terra. A line that waits more than 10 s for a translator is shown untranslated |
| The first subtitle after starting is slow | The model is loading (built-in about 7 s, Ollama about 13 s, Claude about 1.3 s, Codex about 5 s). Wait while "…モデル読み込み中" is shown |
| Your Mac uses 50 GB+ of memory | A large Ollama model is loaded. Press "メモリ解放" (Free memory) in Settings (with Ollama as the backend it is also released automatically at start, at exit and on a model switch; what a forced kill left behind is cleared at the next launch) |
| Your Mac's sound suddenly got loud while testing | The app never changes the Mac's output volume or the signal sent to your speakers; the automatic gain control touches only the captured copy. The loudness seen during development came from full-level test sounds (`say`, `afplay`), not from the app's gain |
| You lost the band | A hairline frame always marks its edges, and for the first 10 seconds that frame blinks yellow. Click the band to focus it, then press Esc or the restore button to return to the normal window. Esc works only while the band has focus |
| Claude sign-in does not finish | If the browser flow does not complete by itself, run `claude auth login` in a terminal (the Claude sign-in action itself was not verified on the author's Mac) |
| The app ended unexpectedly | Look at `~/Library/Application Support/LiveSubtitle/crash.log` (panics and termination signals) and at `live-subtitle-*.ips` in `~/Library/Logs/DiagnosticReports/` (macOS crash reports). An exit caused by stopping the app during other work leaves a record too |
| A red "内蔵エンジンのモデル（…）がまだ無い" (the built-in engine has no model yet) appears | Press the "Download" button beside it (about 14.4 GB). To use another GGUF, use "Choose a GGUF file…" in Settings |
| A red "llama-server が起動できなかった" (the server could not start) appears | It is followed by the first error the server wrote (all of its output is in `~/Library/Application Support/LiveSubtitle/llama-server.log`). Usually the chosen GGUF is in a form llama.cpp cannot read, is broken, or there is not enough memory. Go back to the recommended model in Settings |
| `codesign` fails | It fails when the build happens inside an iCloud-synced folder such as `~/Desktop`. `scripts/bundle.sh` builds in a temporary directory, so build through the script |

## Translation backends

| Choice | Measured latency | Notes |
|---|---|---|
| Built-in (the default; the QAT build of Gemma 4 26B) | about 0.5–0.9 s (median 0.75 s over 40 sentences; only the first load takes about 7 s, measured once, right after the file had been read; it is preloaded at start) | local, offline; no other app needed |
| Ollama (its default model is `gemma4:26b-mlx`) | about 0.7–2 s (only the first load takes about 13 s; it is preloaded at start) | local, offline |
| Claude (a resident `claude -p`) | about 0.7–2 s depending on the model (only the first start takes about 1.3 s, measured with Haiku 5.5, during which "翻訳モデル読み込み中" is shown) | high quality; uses your plan's quota. Pick an explicit model version (Haiku 5.5, Sonnet 5.5, Opus 5.5, Fable 5.1) or type a model ID |
| Codex (a resident `codex app-server`) | about 2–5 s (varies a lot; only the first start takes about 5 s) | uses the ChatGPT account the local Codex is signed in to. Models come from `model/list` (what your account can use), and the reasoning effort is selectable. The default on first use is GPT-5.6-Luna at low effort |
| No translation | — | original text only |

## How each backend is connected

The app uses no API keys. The built-in engine starts the server inside the app as a child process. For Claude and Codex the app starts the official commands already installed on your Mac as child processes (the login is whatever those commands already hold).

| Backend | Connection |
|---|---|
| **Claude** | **Starts the `claude` command (CLI).** A `claude -p` process stays running in stream-json mode over stdin/stdout, and each subtitle line is one message to it. Normally there is one; only when speech outpaces the translation are more started, up to three, translating side by side (the extra ones stay until "停止"). The three preceding lines travel with each message so that every process has the context. Thinking, tools, hooks, settings loading and session persistence are all turned off. The history grows with every message, so each process is replaced after 40 lines (the next one is started in the background beforehand). |
| **Codex** | **Starts `codex app-server` (the App Server)** and talks JSON-RPC over stdin/stdout. One server hosts six ephemeral threads, so lines are translated in parallel (`thread/start`, then `turn/start` per line, then it reads `item/agentMessage/delta` and `turn/completed`). Approval policy is `never` and the sandbox is `read-only`. The model list comes from `model/list`; sign-in status and start use `account/read` and `account/login/start`. |
| **Built-in** | **Starts llama.cpp's `llama-server`**, shipped inside the app, and talks to its **HTTP API**. It listens on a free port of `127.0.0.1`, loads one model and translates one line at a time (`-ngl 999 -np 1 -c 4096 --reasoning-budget 0 --no-ui`). Translation is `POST /v1/chat/completions` (`temperature` 0, no thinking); `/health` tells when the model has loaded. |
| **Ollama** | **HTTP API** (default `http://127.0.0.1:11434`, changeable with `OLLAMA_HOST`). Translation is `POST /api/chat` (no streaming, no thinking, `keep_alive` 30 min); the model list is `/api/tags`; memory release uses `/api/ps` and `/api/generate`. |

Codex is reached through the App Server only; neither `codex exec` nor the interactive CLI is used. The built-in engine's server and the Claude and Codex processes are stopped when you press "停止" (stop) and when the app quits.

## The built-in engine

The app can translate locally by itself, without Ollama or anything else. The llama.cpp server inside the app (`Live Subtitle.app/Contents/MacOS/llama-server`) runs a model in GGUF format.

**The model**

- The default is Google's **Gemma 4 26B (the QAT build, 4-bit)**: `gemma-4-26B_q4_0-it.gguf` (about 14.4 GB, Apache 2.0, no sign-in needed to fetch it), published on Hugging Face as [google/gemma-4-26B-A4B-it-qat-q4_0-gguf](https://huggingface.co/google/gemma-4-26B-A4B-it-qat-q4_0-gguf).
- A copy you already have is used, and only a file of the right size. The app looks, in this order, in:
  - this app's folder (`~/Library/Application Support/LiveSubtitle/`)
  - the Hugging Face cache (`~/.cache/huggingface/hub`, or where `HF_HUB_CACHE` or `HF_HOME` point); current llama.cpp builds fetch into it too
  - Ollama's models (`~/.ollama/models`, or where `OLLAMA_MODELS` points), for a model pulled from Hugging Face with Ollama
  - LM Studio's folder (`~/.lmstudio/models`, and `~/.cache/lm-studio/models` of earlier versions) and llama.cpp's earlier cache (`~/Library/Caches/llama.cpp`, or where `LLAMA_CACHE` points)
- The Hugging Face cache and Ollama name a file after the hash (SHA-256) of its content, so the name tells that it is the same file. In LM Studio and the like the file is looked for by name. A models folder moved inside LM Studio is not found (use "Choose a GGUF file…" below).
- Without one, the window shows a "Download (14.4 GB)" button, which fetches the model into `~/Library/Application Support/LiveSubtitle/`. A stopped download continues where it left off when the button is pressed again. When it ends, the size and the checksum (SHA-256) are checked, and a file that does not match is not used.
- To use another model, choose it from the menu in Settings. Beside the recommended model it lists the GGUF files (1 GB or larger) found in the places above, under the name their tool gives them (for example `Ollama: hf.co/…:Q6_K`). "Search again" looks once more. A GGUF somewhere the menu does not cover is picked with "Choose a GGUF file…".
- The other models in the menu are untested, for loading and for quality. One that does not load shows the reason in red.
  - A GGUF that Ollama pulled from Hugging Face (`hf.co/…`) was tried once: it loaded and translated.
  - Some models Ollama distributes itself do not load (`gpt-oss:20b` failed with "unknown model architecture: 'gptoss'", being in Ollama's own format).
  - Ollama's MLX builds (such as `gemma4:26b-mlx`) are not GGUF files and are not listed.

**Memory**

- The model is loaded when you press Start (also when you switch to the built-in engine while running, and when a download ends) and given up on Stop, when the app quits, and when you switch to another backend or model (the server itself is stopped). Once stopped it is not loaded again until the next Start. "Free memory" in Settings only gives it up for now: the next line to translate loads it again.
- While loaded it occupies about 14 GB of physical memory (14.0 GB of RSS in `ps`). Almost all of it is the model file mapped as it is; the memory the process allocated itself (what `footprint` reports) was about 0.5 GB.
- If the app was force-quit and only the server was left, the next launch stops it (only the server recorded in `llama-server.pid`, and only when it has lost its parent).

**What was verified**

- 40 English sentences written in a spoken style were translated in turn by the built-in engine and by Ollama's `gemma4:26b-mlx`, with the app's own prompt and the same preceding lines as context. The median per sentence was 0.75 s for the built-in engine (0.48–0.90) and 0.88 s for Ollama (0.54–1.26); the built-in engine was faster on 35 of the 40 (median difference 0.15 s).
- Quality was compared on the same 40 sentences in these steps (one person judged, and wrote the sentences). The built-in engine (the QAT build) and Ollama were not read against each other blind.
  - Ollama's `gemma4:26b-mlx` and a 4-bit build that is not QAT (Q4_0 from `ggml-org/gemma-4-26B-A4B-it-GGUF`) were read side by side without knowing which wrote which. Sentences where only one was worse: Ollama 4, the Q4_0 build 6. The one clear mistranslation was the Q4_0 build's.
  - The QAT build and the Q4_0 build gave the same text for 30 of the 40 sentences. Of the 10 that differed, only the mistranslated one differed in meaning; the QAT build got the frame of that sentence right (it still dropped "sick of").
  - Every model translated `dry run` as "テスト走行" (a test drive).
  - Every model ended all 40 translations with 。 or ？.
- Automated tests against the real server and model: starting, translating, reusing the server for the same model, the process being gone after a stop, and a server left behind being stopped at launch. Continuing a download from the real host (fetching only the last kilobyte, after which the checksum matches).
- Finding models: unit tests on a made-up set of stores (the recommended model is found wherever Ollama, Hugging Face or LM Studio keeps it; the list of other models and their names). On this Mac the list showed Ollama's two GGUF models and one file in this app's folder, and no MLX model.
- What needs the window (switching backends, the download button and its progress, the model menu and choosing a GGUF, freeing memory) and subtitles from real audio were not checked automatically.

## Releasing Ollama's memory

A large model uses tens of GB of memory. The app frees it the same way [Taceta](https://github.com/mlabo-org/taceta) (a native macOS client for local inference) does with its "release all models" button (ask `/api/ps` which models are loaded, send each one `keep_alive: 0`, and wait until they are gone), at these times. **Models that other apps loaded are included** (they are simply reloaded the next time something uses them). When the backend is not Ollama, the app leaves Ollama's memory alone (unless you press "Free memory" yourself), so that someone who uses Ollama for other work does not have their models unloaded by this app.

- When you press the "メモリ解放" (Free memory) button in Settings
- When the app starts, only if the backend is Ollama (to clear what an abnormal exit left behind)
- When the app quits, only if it asked Ollama for a translation during that run
- When you switch the Ollama model, or switch the engine away from Ollama (the new model is loaded only after the memory is free)
- When the app panics, or is terminated by SIGTERM / SIGINT / SIGHUP, under the same condition as on quitting

If Ollama is not running, the app does not start it (nothing is considered loaded). If the app was killed without a chance to release, the models are cleared at the next launch; until then, Ollama frees them itself once the `keep_alive` (30 minutes) set on the translation requests runs out.

**Verified by hand**

- At launch: `gemma4:26b-mlx` (25.7 GB) and `qwen3.8:27b-mlx` (20.0 GB), which were loaded before launch, were both freed right after launch (the window shows "起動時: Ollama のモデルを 2 個、メモリから解放した").
- At exit: after loading `gpt-oss:20b` (13.7 GB), quitting the app freed it.
- On SIGTERM: likewise freed before the app exited.
- Unit tests against a fake Ollama server: freeing several models, nothing loaded, a model that cannot be freed (its error text is shown), and Ollama not running.
- Pressing the "メモリ解放" button and the release on model switch need UI interaction and were not checked automatically.

**Out of scope**

- The built-in engine's server and the Claude and Codex child processes are not part of this feature; they are stopped separately on "停止" and at exit.
- Memory of anything other than Ollama (such as the whisper model) is not released.

## Environment variables

| Name | Purpose |
|---|---|
| `LIVE_SUBTITLE_MODEL` | path of the whisper model |
| `LIVE_SUBTITLE_CLAUDE` | path of the `claude` command |
| `LIVE_SUBTITLE_CODEX` | path of the `codex` executable (started as `codex app-server`) |
| `OLLAMA_HOST` | where Ollama listens |
| `LIVE_SUBTITLE_LLAMA_SERVER` | path of the built-in engine's `llama-server` (default: beside the app's executable) |
| `HF_HUB_CACHE`, `HF_HOME`, `OLLAMA_MODELS`, `LLAMA_CACHE` | where the built-in engine looks for models (the Hugging Face cache, Ollama's models, llama.cpp's earlier cache); the variables each tool uses are read as they are |
| `LLAMA_SERVER_CACHE` | where `scripts/llama-server.sh` keeps the `llama-server` it built (default `~/Library/Caches/LiveSubtitle`) |
| `LIVE_SUBTITLE_AUTOSTART` | when set, start listening as soon as the app starts |
| `LIVE_SUBTITLE_HISTORY_DIR` | folder for "履歴を保存" (Save) (a folder chosen in Settings takes precedence) |
| `LIVE_SUBTITLE_AUTOBAND` | when set, start in band (caption) mode |
| `LIVE_SUBTITLE_DEBUG_LOG` | append input level, segmentation, recognition and translation timings to this path |

## Layout

- `src/capture.rs` system audio capture (ScreenCaptureKit; the app's own sound is excluded)
- `src/agc.rs` automatic gain control (fast attack, slow release envelope)
- `src/history.rs` saving the conversation to a file (only when the button is pressed)
- `src/pipeline.rs` utterance splitting (Silero VAD finds the speech, forced split at 10 s), whisper, translation threads
- `src/sentences.rs` joins recognized utterances into sentences before they are translated
- `assets/vad/` the voice activity detection model (embedded in the app)
- `src/translate.rs` backend switching and Ollama
- `src/builtin.rs` the built-in engine (starting and stopping the bundled llama.cpp server, finding models in the stores of other tools, downloading)
- `src/claude.rs` translation through a resident Claude CLI
- `src/codex.rs` translation through a resident Codex App Server
- `src/main.rs` GUI
- `src/app_shell_foundation.rs` shared display-settings component (managed by app-shell-foundation; do not edit)
- `examples/capture-spike.rs` an audio-capture-only smoke test
- `scripts/bundle.sh` builds, creates, signs and installs the `.app`
- `scripts/llama-server.sh` builds the built-in engine's `llama-server` from llama.cpp at a pinned release (called by `bundle.sh`)
- `assets/icon/` the app icon (the source picture `source.png`, `AppIcon.icns` for the bundle, and `window-icon-512.png` used while running); rebuilt with `scripts/make-icon.sh`
- `AGENTS.md` the install procedure and the development rules for coding agents

## Known limitations

- On this Mac the captured audio is quite quiet (RMS about 0.005); the cause is unknown. Automatic gain control (AGC: target about -20 dBFS, at most +36 dB, gain held during silence) lifts it before whisper. Utterance splitting does not go by loudness: a voice activity detection model (Silero VAD) finds the speech in the same gain-corrected audio. Loud ambient sound such as wind or a crowd is not treated as speech.
- Stretches without a voice are not handed to whisper, but the voice activity detection can take music for speech (even without singing). The hallucinated speech that follows is suppressed by the no-speech probability and by dropping symbol-only results, but not perfectly.

## License

[MIT](LICENSE)

The app ships [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` (MIT License); the full licence text goes into the `.app` as `Contents/Resources/LLAMA_CPP_LICENSE`. The built-in engine's model (Gemma 4, Apache 2.0) is not shipped; it is fetched onto the user's Mac.

The bundled voice activity detection model (`assets/vad/ggml-silero-v5.1.2.bin`) is [Silero VAD](https://github.com/snakers4/silero-vad) (MIT License) in whisper.cpp's format, taken from [ggml-org/whisper-vad](https://huggingface.co/ggml-org/whisper-vad).
