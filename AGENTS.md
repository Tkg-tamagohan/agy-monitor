# agy-monitor 開発規約と不変条件

このリポジトリで実装、修正、テスト、レビューを行うときは、以下の技術制約および不変条件を順守する。

## リポジトリ概要と技術制約

- Pure Rust 構成（`eframe 0.37`、`egui_plot 0.38`、`rusqlite 0.41` の `bundled` 機能）を維持し、外部ランタイムや追加のネイティブ依存を増やさない。
- 外部ネットワーク通信を行わない完全オフライン動作を維持する。
- 料金換算や課金額のコスト表示機能を追加しない。

## Read-Only SQLite 原則

- `~/.gemini/` 配下（`antigravity`、`antigravity-cli`、`antigravity-ide`、`antigravity-backup` 内の `.db` や関連ファイル）への書き込みを厳禁とする。
- `crates/agy-core/src/sqlite.rs` の `open_readonly` が用いる `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` と `PRAGMA query_only = ON` を維持し、URI パラメータの `immutable=1` は使わない。

## 監視ワーカーとモデルラベルキャッシュの不変条件

- `PRG-001`：`crates/agy-core/src/monitor.rs` において `antigravity_state.pbtxt` 由来のフォールバックモデル名は詳細ペイン表示のみに留め、サイドバー用 `model_label_cache` へ書き込まない。
  DB ロック時や非 DB 由来結果で既存の DB 由来キャッシュを上書きまたは消去しない。
- `PRG-002`：50ms モデルバックフィルティックはキャッシュ済み `SessionIndex` を再利用して `load_session_index` を呼ばず、同一ポール周期で `busy` となったセッションは `busy_this_poll` で次回通常ポールまでスキップし、`has_more` による無限スピンを防ぐ。
- `PRG-004`：`populate_session_model_labels` の選択セッション最終同期は `can_seed_selected && !selected_db_busy` で保護し、ロック再試行抑止テスト（`crates/agy-core/tests/spec_tests.rs`）では先頭非選択ロック DB と残枠飽和件数、`model_scan_count`、通知回数を厳密に検証する。

## GUI レイアウトと日本語フォントの不変条件

- `PRG-003`（フォント）：`crates/agy-gui/src/fonts.rs` では `egui` 既定欧文フォントを先頭に保ち、日本語フォントをフォールバック（`.push`）として末尾へ登録する。
  `YuGothR.ttc` は行高が過大になるため `CANDIDATE_FONTS` へ含めない。
- `PRG-003`（レイアウト）：狭幅または低画面時は下部詳細タブ用の予約高さを先取りし、上部モニターに上限スクロールを適用する（`crates/agy-gui/src/app.rs`）。
- `PRG-003`（フォント回帰検査）：`CANDIDATE_FONTS` を追加または変更するときは、全実在フォント候補に対してフォールバック登録、行高、垂直中心の回帰検査（`gui_10_all_existing_candidate_fonts_render_properly`）を通す。

## 検証コマンドと Windows 開発環境規約

- コードや設定を変更したあとは、次の通常検証コマンドをすべて通す。

```powershell
cargo fmt --all -- --check; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo clippy --workspace --all-targets --locked -- -D warnings; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --workspace --locked; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

- `smoke_01_readonly_real_gemini_data` は実 `~/.gemini/antigravity` 依存の `#[ignore]` テストであるため、通常テストおよび CI 実行対象からは除外した状態を保つ。
- Windows PowerShell 5.1 では `&&` を使わず `; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }` を用い、日本語を含むコミットメッセージと PR 本文は BOM なし UTF-8 の一時ファイル経由で渡す。

## ドキュメント記述とコード照合の原則

- `PR #3 PRG-001`：動作環境や SQLite 再試行の適用範囲、キャッシュ対象範囲を文書へ記載するときは、Edition や共通ヘルパーの存在だけから一律に推測しない。
  全呼び出し元とクロージャ内のエラー伝播、および CI ワークフロー定義を実コードと突き合わせて確認し、適用対象と例外を明記する。
