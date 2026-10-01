# live-subtitle Local Constitution

このファイルは、このリポジトリ配下における局所 `AGENTS.md` であり、本スコープ内の実行条件を定義する SSOT である。
上位の `AGENTS.md`、システム指示、開発者指示、ユーザーの明示要求と競合する場合は、その優先順位規則に従う。

## Scope

- Mac の全システム音声を取得し、whisper.cpp で書き起こし、日本語字幕にする Rust と `eframe/egui` の macOS GUI アプリ。
- このリポジトリのソースが正本。`~/Applications/Live Subtitle.app` は実行用の生成物で、ソースと同一視しない。ソースを動かしても、インストール済みアプリが更新されたことにはならない。
- `src/app_shell_foundation.rs` と `.app-shell-foundation/` は app-shell-foundation スキルの管理下にある。編集しない（更新はスキルの `apply` で行う）。

## Path Contract

- 起動、ビルド、検証、README の参照は、リポジトリのルートからの相対パスで書く。ホーム配下や旧実験場所の絶対パスを入れない。
- 例外は、リポジトリ外の OS のパス（`/usr/lib/swift` など）と、`~`・`$HOME` 基準の実行時の置き場（モデルは `~/Library/Application Support/LiveSubtitle/`）。

## Build And Runtime

- ビルドとアプリ化は `scripts/bundle.sh` だけで行う。ビルドは一時ディレクトリで行い、作業ツリーに `target/` を作らない。エディタの解析が作った `target/` は、コミットや引き渡しの前に消す。
- `CODESIGN_IDENTITY` に固定の署名 ID を渡す。ad-hoc 署名だと、ビルドし直すたびに画面収録の許可が外れる。
- 通常の実行は、できあがった `.app` を直接開く。`cargo run` は使わない。
- whisper のモデル（約 574 MB）はリポジトリに入れない。

## High-Risk Boundaries

- 画面収録の許可（TCC）は、ユーザーがシステム設定で付ける。署名を変えたあとに拒否される場合は、一覧の項目を削除して登録し直してもらう。こちらから設定を変えない。
- 取得した音声の音量は小さい（RMS 約 0.005）。発話の判定は相対しきい値と音量そろえに頼っている。固定の絶対しきい値を入れると、字幕が一切出なくなる。

## Validation

- 主要経路の確認は、英語など日本語以外の音声を流して、原文と日本語訳の両方が字幕に出ること。診断には `LIVE_SUBTITLE_DEBUG_LOG` と `LIVE_SUBTITLE_AUTOSTART` を使う（README 参照）。
- 画面の変更は、アプリを起動して表示を見て確認する。
