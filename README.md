# Live Subtitle

Mac で鳴っているすべての音声（YouTube・X・会議など、アプリを問わず）を聞き取り、日本語字幕にする macOS 用 GUI ツール（Rust / eframe・egui）。

```
システム音声 ─ ScreenCaptureKit ─ 発話の区切り検出 ─ whisper.cpp (Metal) ─ 翻訳 ─ 字幕
                 16 kHz mono        相対しきい値      言語自動判定            Ollama / Claude
```

- 原文はすぐ表示し、日本語訳は完成し次第あとから差し込む（翻訳が遅くても字幕は止まらない）。
- 日本語の発話は翻訳せずそのまま出す。
- 画面上部に dB メーターを出す。入力が届いていれば数値、無音なら「無音」と表示する。
- モデル読み込み中（whisper・Ollama）は「…モデル読み込み中」と表示する。
- ウィンドウは既定で最前面に固定（トグルで切り替え）。

## 必要なもの

- macOS 13 以降、Apple Silicon、Rust、cmake
- whisper のモデル `ggml-large-v3-turbo-q5_0.bin`（約 574 MB）を `~/Library/Application Support/LiveSubtitle/` に置く
  （取得元: `https://huggingface.co/ggerganov/whisper.cpp`。別の場所は `LIVE_SUBTITLE_MODEL` で指定）
- 翻訳に Ollama を使うなら、Ollama が動いていてモデルが入っていること
- 翻訳に Claude を使うなら、`claude` コマンド（`~/.local/bin` など。別の場所は `LIVE_SUBTITLE_CLAUDE`）

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
| Claude（`claude -p`） | 約 6〜10 秒 | 高品質。プラン枠を使う |
| 翻訳しない | — | 原文のみ |

## 環境変数

| 名前 | 用途 |
|---|---|
| `LIVE_SUBTITLE_MODEL` | whisper モデルのパス |
| `LIVE_SUBTITLE_CLAUDE` | `claude` コマンドのパス |
| `OLLAMA_HOST` | Ollama の接続先 |
| `LIVE_SUBTITLE_AUTOSTART` | 設定すると起動と同時に聞き取りを始める |
| `LIVE_SUBTITLE_DEBUG_LOG` | 設定したパスに、入力レベル・区切り・認識・翻訳時間を追記する |

## 構成

- `src/capture.rs` システム音声の取得（ScreenCaptureKit、自アプリの音は除外）
- `src/pipeline.rs` 区切り検出（相対しきい値・最大 10 秒で強制分割）、whisper、翻訳スレッド
- `src/translate.rs` Ollama / Claude の翻訳
- `src/main.rs` GUI
- `src/app_shell_foundation.rs` 表示設定の共通部品（app-shell-foundation が管理。編集しない）
- `examples/capture-spike.rs` 音声取得だけの動作確認

## 既知の制約

- このMacでは、取得した音声の音量がかなり小さい（RMS 約 0.005）。原因は未特定。区切り検出は相対しきい値と音量そろえで対応している。
- 音楽や環境音しかない区間でも、10 秒ごとに whisper へ渡す。幻聴（存在しない発話の出力）は、無音確率と記号だけの結果の除外で抑えているが、完全ではない。
