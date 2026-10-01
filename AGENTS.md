# live-subtitle Local Constitution

このファイルは、このリポジトリ配下における局所 `AGENTS.md` であり、本スコープ内の実行条件を定義する SSOT である。
上位の `AGENTS.md`、システム指示、開発者指示、ユーザーの明示要求と競合する場合は、その優先順位規則に従う。

## Scope

- Mac の全システム音声を取得し、whisper.cpp で書き起こし、日本語字幕にする Rust と `eframe/egui` の macOS GUI アプリ。
- このリポジトリのソースが正本。`~/Applications/Live Subtitle.app` は実行用の生成物で、ソースと同一視しない。ソースを動かしても、インストール済みアプリが更新されたことにはならない。
- `src/app_shell_foundation.rs` と `.app-shell-foundation/` は app-shell-foundation スキルの管理下にある。編集しない（更新はスキルの `apply` で行う）。
- README は `README.md`（英語）と `README.ja.md`（日本語）の 2 つを、同じ内容で保つ。片方を変えたら、もう片方も変える。ライセンスは MIT（`LICENSE`、`Cargo.toml` の `license`）。

## Path Contract

- 起動、ビルド、検証、README の参照は、リポジトリのルートからの相対パスで書く。ホーム配下や旧実験場所の絶対パスを入れない。
- 例外は、リポジトリ外の OS のパス（`/usr/lib/swift` など）と、`~`・`$HOME` 基準の実行時の置き場（モデルは `~/Library/Application Support/LiveSubtitle/`）。

## Build And Runtime

- ビルドとアプリ化は `scripts/bundle.sh` だけで行う。ビルドは一時ディレクトリで行い、作業ツリーに `target/` を作らない。エディタの解析が作った `target/` は、コミットや引き渡しの前に消す。
- `CODESIGN_IDENTITY` に固定の署名 ID を渡す。ad-hoc 署名だと、ビルドし直すたびに画面収録の許可が外れる。
- 通常の実行は、できあがった `.app` を直接開く。`cargo run` は使わない。
- whisper のモデル（約 574 MB）はリポジトリに入れない。
- アイコンは `assets/icon/source.png` が元の絵。`scripts/make-icon.sh` で `assets/icon/AppIcon.icns`（`.app` のアイコン）と `assets/icon/window-icon-512.png`（実行中に Dock へ渡すアイコン）を作り直す。後者を `src/main.rs` が `include_bytes!` で読み込んでいる。渡さないと、実行中だけ eframe の既定アイコンになる。
- アプリを終了するときは、認識のスレッドを先に止めて whisper のモデルを解放してからプロセスを終える（`pipeline::finish_asr`）。モデルが残ったまま終了すると、ggml の Metal の後片付けが `abort` してクラッシュの記録が残る。
- 予期しない終了（パニック、終了の合図）は `~/Library/Application Support/LiveSubtitle/crash.log` に残る。macOS のクラッシュの記録は `~/Library/Logs/DiagnosticReports/` にある。

## Install Procedure For Agents

ユーザーが「インストールして」「ビルドして」「更新して」と頼んだときの手順。各ステップは、読み取りの確認を先に行い、満たしていれば飛ばす。リポジトリのルートで作業する。

1. **前提の確認（読み取りだけ）**: `sw_vers -productVersion`（13 以上）、`uname -m`（`arm64`）、`xcrun --find swift`、`cargo --version`、`cmake --version`。欠けているものは、ユーザーに報告し、入れてよいか確認してから入れる（`xcode-select --install`、`brew install cmake`、Rust は rustup）。黙って入れない。
2. **whisper のモデル（約 574 MB のダウンロード）**: `test -f ~/"Library/Application Support/LiveSubtitle/ggml-large-v3-turbo-q5_0.bin"` で確認する。無ければ、ファイル名・取得元（`huggingface.co/ggerganov/whisper.cpp`）・サイズをユーザーに示して許可を得てから、README の `curl` コマンドで取得する。
3. **署名 ID**: `security find-identity -v -p codesigning` で有効な ID を調べる。1 つならそれを使ってよいかユーザーに確認し、複数なら選んでもらう。無ければ、ad-hoc 署名になり、ビルドし直すたびに画面収録の許可が外れる、とユーザーに伝える。署名 ID を、更新のたびに変えない（許可が外れる）。
4. **実行中のアプリの扱い**: `pgrep -f "Live Subtitle.app/Contents/MacOS"` で調べる。実行中なら、止める前に**必ずユーザーへ知らせる**（使用中の字幕が消える）。止めるときは `osascript -e 'tell application "Live Subtitle" to quit'` を使う（`pkill` や `kill` は使わない。終了時にモデルのメモリ解放を行う経路を通すため）。
5. **ビルドとインストール**: `CODESIGN_IDENTITY="<署名 ID>" scripts/bundle.sh`。出力は `~/Applications/Live Subtitle.app`。ビルドは一時ディレクトリで行う。`cargo run` は使わない。失敗したら、README の「つまずいたときは」を見る（iCloud 同期下の `codesign` 失敗、Swift の欠如が多い）。
6. **起動の確認**: `open ~/Applications/"Live Subtitle.app"` のあと、数秒待って `pgrep -f "Live Subtitle.app/Contents/MacOS"` で動いていることを確かめる。
7. **翻訳先の準備（ユーザーが使うものだけ）**: Ollama は `curl -s localhost:11434/api/tags` で動作とモデルを確認する。モデルが無ければ、`ollama pull gemma4:26b-mlx` を提案する（約 18 GB のダウンロードなので、許可を得る）。Claude は `claude` コマンド、Codex は `codex` 実行ファイル（アプリは `codex app-server` として起動する。`codex exec` や対話式の CLI は使わない）が在ることの確認だけを行う。
8. **ユーザーに頼むこと（エージェントは行わない）**: 画面収録とシステムオーディオ録音の許可（システム設定）、Claude／ChatGPT のサインイン（画面のボタン）、フォルダへのアクセスの確認ダイアログ。システム設定を変えない。認証情報を扱わない。サインインのボタンを押さない。許可の手順は README の「macOS の許可」を案内する。
9. **完了の報告**: インストール先、起動の確認結果、ユーザーに残っている手作業（上の 8）を伝える。ソースの変更だけで、アプリを入れ替えていないときは、そう報告する（ソース・ビルド・入れ替えは別の段階）。

## High-Risk Boundaries

- 画面収録の許可（TCC）は、ユーザーがシステム設定で付ける。署名を変えたあとに拒否される場合は、一覧の項目を削除して登録し直してもらう。こちらから設定を変えない。
- 取得した音声の音量は小さい（RMS 約 0.005）。whisper に渡す音は `src/agc.rs` の自動音量補正で持ち上げ、発話の判定は補正前の信号への相対しきい値で行う。固定の絶対しきい値を入れたり、補正前の信号を whisper に直接渡したりすると、字幕が出なくなる。

- 増幅（自動音量補正を含む）は、取り込んだ音声の複製にアプリの内部でだけ掛ける。システムの出力音量、出力デバイス、ユーザーのスピーカーやヘッドホンへの信号には触れない。音量設定の API や音の出力を、このアプリに入れない。
- 動作確認でスピーカーから音を出す（`say`、`afplay` など）ときは、先にユーザーへ断る。テスト音はフルレベルで、動画の音よりずっと大きく聞こえる。

## Validation

- 主要経路の確認は、英語など日本語以外の音声を流して、原文と日本語訳の両方が字幕に出ること。診断には `LIVE_SUBTITLE_DEBUG_LOG` と `LIVE_SUBTITLE_AUTOSTART` を使う（README 参照）。
- 画面の変更は、アプリを起動して表示を見て確認する。
