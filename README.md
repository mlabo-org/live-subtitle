# Live Subtitle

Mac で鳴っているすべての音声（YouTube・X・会議など、アプリを問わず）を聞き取り、日本語字幕にする macOS 用 GUI ツール（Rust / eframe・egui）。

```
システム音声 ─ ScreenCaptureKit ─ 自動音量補正 ─ 発話の区切り検出 ─ whisper.cpp (Metal) ─ 翻訳 ─ 字幕
                 16 kHz mono       (AGC)          相対しきい値      言語自動判定            Ollama / Claude
```

- 原文はすぐ表示し、日本語訳は完成し次第あとから差し込む（翻訳が遅くても字幕は止まらない）。
- 日本語の発話は翻訳せずそのまま出す。
- 画面上部に入力音声のメーターを出す（緑、大きいと黄色、無音は灰色で「無音」）。表示は自動音量補正のあとの音量で、バーに乗せると補正量（dB）が出る。
- 「会話履歴を保存」を押したときだけ、いまの字幕（原文・訳・時刻）を日時つきのテキストファイルに保存して、Finder で表示する（既定では何も保存しない）。保存先は既定でデスクトップ（ファイル名は `Live Subtitle 日付 時刻.txt`）。「保存先を選ぶ…」で任意のフォルダに変えられ、選んだ場所は次回も覚えている（「デスクトップに戻す」で既定に戻る）。初回は、そのフォルダへのアクセス許可を求められることがある。
- モデル読み込み中（whisper・Ollama）は「…モデル読み込み中」と表示する。
- ウィンドウは既定で最前面に固定（トグルで切り替え）。
- 「帯にする」で、字幕だけの軽量表示（テロップ）になる。タイトルバーのない半透明の帯で、ドラッグで好きな位置へ動かし、端でサイズを変えられる（字幕の文字は帯の高さに合わせて大きくなる）。初回は画面の下部に出て、位置とサイズは次回も覚えている。しばらく発話が無いと消え、全画面表示の動画の上にも重なる。**ESC**（または帯に出る「元に戻す」ボタン）で、元のサイズと位置の通常画面に戻る。ESC は帯にフォーカスがあるときだけ効く（帯をクリックするとフォーカスされる）。

## 必要なもの

- macOS 13 以降、Apple Silicon、Rust、cmake
- whisper のモデル `ggml-large-v3-turbo-q5_0.bin`（約 574 MB）を `~/Library/Application Support/LiveSubtitle/` に置く
  （取得元: `https://huggingface.co/ggerganov/whisper.cpp`。別の場所は `LIVE_SUBTITLE_MODEL` で指定）
- 翻訳に Ollama を使うなら、Ollama が動いていてモデルが入っていること
- 翻訳に Claude を使うなら、`claude` コマンド（`~/.local/bin` など。別の場所は `LIVE_SUBTITLE_CLAUDE`）
- 翻訳に Codex を使うなら、`codex` コマンド（別の場所は `LIVE_SUBTITLE_CODEX`）
- Claude と Codex は、サインインしていなければ、画面の「サインイン」ボタンから公式のブラウザ認証（OAuth）を始められる。アプリは認証情報を扱わず、状態の確認と、公式の手順の開始だけを行う（Claude は `claude auth`、Codex は App Server の `account/*`）。

## ビルドと起動

```bash
CODESIGN_IDENTITY="<署名 ID>" scripts/bundle.sh   # 既定の出力先: ~/Applications/Live Subtitle.app
```

ビルドは一時ディレクトリで行う（iCloud 同期下だと codesign が失敗するため）。
`CODESIGN_IDENTITY` を省くと ad-hoc 署名になり、**ビルドし直すたびに画面収録の許可が外れる**。固定の署名 ID を使うこと。

初回は、システム設定 → プライバシーとセキュリティ → 画面収録とシステムオーディオ録音 で `Live Subtitle.app` を許可する。
署名を変えたあとに許可しているのに拒否される場合は、一覧から項目を「−」で削除して登録し直す。

## 翻訳先

| 選択 | 実測の遅延 | 備考 |
|---|---|---|
| Ollama（既定 `gemma4:26b-mlx`） | 約 0.7〜2 秒（初回の読み込みだけ約 13 秒。起動時に先読みする） | ローカル・オフライン |
| Claude（常駐した `claude -p`） | 約 0.7〜2 秒（モデル次第。初回の起動だけ約 6 秒で、その間は「翻訳モデル読み込み中」と出る） | 高品質。プラン枠を使う。モデルは版を明示した ID（Haiku 4.5・Sonnet 5.5・Opus 5.5・Fable 5.1）から選ぶか、ID を手入力する |
| Codex（常駐した `codex app-server`） | 約 2〜5 秒（ばらつきが大きい。初回の起動だけ約 5 秒） | ローカル Codex にサインイン済みの ChatGPT アカウントを使う。モデルは `model/list`（アカウントで使える一覧。既定は GPT-6.1-Sol）から選び、考える強さも選べる |
| 翻訳しない | — | 原文のみ |

## Ollama のメモリ解放

大きなモデルは、メモリを数十 GB 使う。Taceta の「全モデル解放」と同じ方法（`/api/ps` で載っているモデルを調べ、各モデルに `keep_alive: 0` を送り、メモリから消えるまで待つ）で、次のときに解放する。**他のアプリが読み込んだモデルも対象**になる（次に使われたとき、再読み込みされる）。

- 画面の「メモリ解放」ボタンを押したとき
- アプリの起動時（異常終了の取りこぼしを片付ける）
- アプリの終了時
- Ollama のモデルを切り替えたとき、または翻訳先を Ollama から別のものに変えたとき（新しいモデルは、解放が終わってから読み込む）
- アプリがパニックしたとき、または SIGTERM／SIGINT／SIGHUP で終了させられたとき

Ollama が起動していなければ、起動はしない（載っているモデルはないものとして扱う）。強制終了などで、解放する機会がなかった場合は、次の起動時に片付く。それまでも、翻訳のリクエストに付けた `keep_alive`（30 分）を過ぎれば、Ollama が自分で解放する。

## 環境変数

| 名前 | 用途 |
|---|---|
| `LIVE_SUBTITLE_MODEL` | whisper モデルのパス |
| `LIVE_SUBTITLE_CLAUDE` | `claude` コマンドのパス |
| `LIVE_SUBTITLE_CODEX` | `codex` コマンドのパス |
| `OLLAMA_HOST` | Ollama の接続先 |
| `LIVE_SUBTITLE_AUTOSTART` | 設定すると起動と同時に聞き取りを始める |
| `LIVE_SUBTITLE_HISTORY_DIR` | 「会話履歴を保存」の保存先（画面で選んだ場所があれば、そちらが優先） |
| `LIVE_SUBTITLE_AUTOBAND` | 設定すると起動と同時に帯（テロップ）表示にする |
| `LIVE_SUBTITLE_DEBUG_LOG` | 設定したパスに、入力レベル・区切り・認識・翻訳時間を追記する |

## 構成

- `src/capture.rs` システム音声の取得（ScreenCaptureKit、自アプリの音は除外）
- `src/agc.rs` 自動音量補正（速く立ち上がり、ゆっくり戻る包絡線）
- `src/history.rs` 会話履歴のファイル保存（ボタンを押したときだけ）
- `src/pipeline.rs` 区切り検出（相対しきい値・最大 10 秒で強制分割）、whisper、翻訳スレッド
- `src/translate.rs` 翻訳先の切り替えと Ollama
- `src/claude.rs` 常駐した Claude CLI での翻訳
- `src/codex.rs` 常駐した Codex App Server での翻訳
- `src/main.rs` GUI
- `src/app_shell_foundation.rs` 表示設定の共通部品（app-shell-foundation が管理。編集しない）
- `examples/capture-spike.rs` 音声取得だけの動作確認

## 既知の制約

- このMacでは、取得した音声の音量がかなり小さい（RMS 約 0.005）。原因は未特定。自動音量補正（AGC。目標は約 -20 dBFS、最大 +36 dB、無音の間はゲインを固定）で持ち上げて whisper に渡す。区切り検出は、補正前の信号に対する相対しきい値で行う。
- 音楽や環境音しかない区間でも、10 秒ごとに whisper へ渡す。幻聴（存在しない発話の出力）は、無音確率と記号だけの結果の除外で抑えているが、完全ではない。
