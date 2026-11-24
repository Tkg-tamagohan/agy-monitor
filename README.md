# agy-monitor

`agy-monitor` は、Antigravity のローカルデータ（`~/.gemini/` 配下）を読み取り専用で監視し、コンテキスト使用量、キャッシュ率、ステップ明細、重いツール呼び出しをリアルタイムに表示する Windows 向けネイティブ GUI モニター（ウィンドウタイトル `Antigravity Monitor`）である。
`eframe 0.37`、`egui_plot 0.38`、`rusqlite 0.41`（`bundled` 機能）を使った Pure Rust 構成を採用し、外部ランタイムや外部ネットワーク通信を使わずに完全オフラインで動作する。
料金換算や課金額のコスト表示機能は設けていない。

## 主な機能

- **リアルタイム監視**：`~/.gemini/` 配下の `antigravity`、`antigravity-cli`、`antigravity-ide`、`antigravity-backup` と `projects.json` を既定 2 秒間隔で監視し、選択中セッションの状態を更新する。
- **KPI と推移グラフ**：コンテキスト使用率、直近キャッシュ率、累計キャッシュ率、Compaction 回数をカードで表示し、コンテキスト推移とキャッシュ率推移をグラフに描画する。
- **詳細タブ**：並び順切替と Compaction 表示を備えたステップ明細テーブルと、負荷の大きい呼び出しをまとめた一覧（`重いツール Top 15`）を切り替えて参照できる。
- **セッション一覧と検索**：サイドバーでプロジェクト絞り込みやキーワード検索（短縮モデル名を含む）を行い、会話ごとの利用モデルと状態を確認できる。
  手動で会話を選択すると自動追従が解除され、トップバーの「最新追従」から再度有効化できる。
- **表示とウィンドウ操作**：通常表示（初期サイズ `1240x820`、最小 `760x520`）とコンパクト HUD（`380x170`）の切替、最前面固定、ダークとライトのテーマ切替、監視の一時停止と手動更新を備える。
- **未実装タブ（Phase 2 予定）**：トップバーに「期間統計 (Phase 2)」タブを配置しているものの、期間統計、カスタマイズと MCP 棚卸しの実装は Phase 2 予定のためプレースホルダ表示にとどまる。

## 動作環境と起動方法

本リポジトリは Rust Edition 2024 と `let` チェーン構文（Rust 1.88 以降）および最新系クレートを使用しており、`Cargo.toml` で MSRV（`rust-version`）は固定していない。
ビルドには最新の stable Rust ツールチェーン（CI は `dtolnay/rust-toolchain@stable`、ローカルでの動作確認済み環境は Windows 上の Rust 1.100.0）を利用する。
日本語表示では Windows 標準フォント（`BIZ-UDGothicR.ttc`、`meiryo.ttc`、`NotoSansJP-VF.ttf`、`YuGothM.ttc`、`msgothic.ttc` の優先順）をフォールバックとして読み込む設計上、現時点の主対象環境と CI 検証対象は Windows である。

ビルドと起動には次のコマンドを実行する。

```powershell
cargo build --release -p agy-gui --bin agy-monitor
cargo run --release -p agy-gui --bin agy-monitor
```

コマンドライン引数は不要で、起動時にホームディレクトリ配下の `~/.gemini` を自動検出する（取得できない場合はカレントディレクトリの `.gemini` を参照する）。

## 読み取り専用アクセスと対象データ

`~/.gemini/` 配下のデータベースおよびログファイルへは一切書き込まない。
SQLite ファイル（`conversation_summaries.db`、`conversations/<conv_id>.db`）はすべて `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`、`PRAGMA query_only = ON`、ビジータイムアウト（150ms）で開く。
競合時にビジーまたはロックエラーが返る読み取り経路（`conversation_summaries.db` の一覧取得と選択中セッションの解析）では、バックオフ付き再試行（30ms、80ms、160ms）を行う。
ただし、サイドバーの一覧表示で使う軽量なモデル名取得では再試行を行わずビジー時にその周期をスキップする。
会話ログ（`transcript_full.jsonl`、`transcript.jsonl`）と状態ファイル（`antigravity_state.pbtxt`）も読み取り専用で参照する。

## リポジトリ構成

- `crates/agy-core`：パス解決、SQLite 読み取り、会話ログ解析、モデル判定、バックグラウンド監視ワーカーを担うコアライブラリ。
- `crates/agy-gui`：`eframe` と `egui_plot` によるネイティブ GUI、日本語フォント設定、バイナリ `agy-monitor` を含むフロントエンドクレート。
- `AGENTS.md`：リポジトリの開発規約と不変条件。
- `.github/workflows/ci.yml`：変更検出、テキスト衛生検査（UTF-8、`U+FFFD`、マージ競合マーカー）、Windows ランナーでのフォーマット、Clippy、テスト実行を定義した GitHub Actions ワークフロー。

## 開発と検証

コードや設定を変更したときは、`AGENTS.md` に定めた通常検証コマンドをすべて通す。

```powershell
cargo fmt --all -- --check; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo clippy --workspace --all-targets --locked -- -D warnings; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --workspace --locked; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

実データ向けスモークテスト（`crates/agy-core/tests/spec_tests.rs` の `smoke_01_readonly_real_gemini_data`）はローカル環境の `~/.gemini/antigravity` に依存する `#[ignore]` テストであるため、通常テストおよび CI の実行対象からは除外されている。
手元の実データで動作確認を行う場合のみ、次のコマンドで個別実行する。

```powershell
cargo test -p agy-core --test spec_tests smoke_01_readonly_real_gemini_data -- --ignored
```

開発時の詳細な不変条件と実装上の制約については、[AGENTS.md](AGENTS.md) にまとめている。

## ライセンス

`Cargo.toml` の `license = "MIT"` に基づき MIT License で提供する。
