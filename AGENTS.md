# agy-monitor 開発規約と不変条件

このリポジトリで実装、修正、テスト、レビューを行うときは、以下の技術制約および不変条件を順守する。

## リポジトリ概要と技術制約

- Pure Rust 構成（`eframe 0.37`、`egui_plot 0.38`、`rusqlite 0.41` の `bundled` 機能）を維持し、外部ランタイムや追加のネイティブ依存を増やさない。
- 外部ネットワーク通信を行わない完全オフライン動作を維持する。
- 料金換算や課金額のコスト表示機能を追加しない。

## Read-Only SQLite 原則

- `~/.gemini/` 配下（`antigravity`、`antigravity-cli`、`antigravity-ide`、`antigravity-backup` 内の `.db` や関連ファイル）への書き込みを厳禁とする。
- `crates/agy-core/src/sqlite.rs` の `open_readonly` が用いる `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` と `PRAGMA query_only = ON` を維持し、URI パラメータの `immutable=1` は使わない。
- `PR #4 PRG-001`：`crates/agy-core/src/sqlite.rs` の再試行関数群（`with_readonly_retry`、`with_readonly_retry_schedule`、`with_readonly_retry_schedule_with_hook`）のクロージャ内では、SQLite クエリの準備、実行、行取得（`prepare`、`query`、`query_map`、`next`）のエラーを `.unwrap_or` や `.filter_map(|r| r.ok())` で握りつぶさず `?` で伝播させ（列値デコードの既定値化やスキップを除く）、再試行の重複蓄積を防ぐためクロージャ先頭で可変蓄積変数（例：`models.rs` の `found_slugs` と `agy_max_token_limit`）を初期化する。
  再試行を行わない唯一の例外は `crates/agy-core/src/models.rs` の `detect_conversation_model_label_from_db`（`with_readonly_retry_schedule` へ空配列 `&[]` を渡す軽量取得）に限定する。

## 監視ワーカーとモデルラベルキャッシュの不変条件

- `PRG-001`：`crates/agy-core/src/monitor.rs` において `antigravity_state.pbtxt` 由来のフォールバックモデル名は詳細ペイン表示のみに留め、サイドバー用 `model_label_cache` へ書き込まない。
  DB ロック時や非 DB 由来結果で既存の DB 由来キャッシュを上書きまたは消去しない。
- `PRG-002`：50ms モデルバックフィルティックはキャッシュ済み `SessionIndex` を再利用して `load_session_index` を呼ばず、同一ポール周期で `busy` となったセッションは `busy_this_poll` で次回通常ポールまでスキップし、`has_more` による無限スピンを防ぐ。
- `PRG-004`：`populate_session_model_labels` の選択セッション最終同期は `can_seed_selected && !selected_db_busy` で保護し、ロック再試行抑止テスト（`crates/agy-core/tests/spec_tests.rs`）では先頭非選択ロック DB と残枠飽和件数、`model_scan_count`、通知回数を厳密に検証する。
- `PR #5 PRG-002`：期間統計と棚卸しの非同期実行は専用スレッド（`agy-monitor-phase2`）へ分離し、手動更新を除く通常 2 秒ポーリングの `index_reload_count` と通知回数に影響を与えない。
  `AnalysisCacheKey` に `breakdown_version` を含めて `RefreshNow` と `RefreshCustomizations` による内訳更新時にリアルタイム解析を無効化し、手動の `RefreshUsage` は一時停止中でも最新の `SessionIndex` を読み直し、`MonitorCommand::Shutdown` 送信時と `MonitorHandle::drop` 時はキャンセルフラグ更新と `Phase2Cmd::Shutdown` 送信を行い、`drop` 時のみ `thread` と `phase2_thread` を `join` する。

## GUI レイアウトと日本語フォントの不変条件

- `PRG-003`（フォント）：`crates/agy-gui/src/fonts.rs` では `egui` 既定欧文フォントを先頭に保ち、日本語フォントをフォールバック（`.push`）として末尾へ登録する。
  `YuGothR.ttc` は行高が過大になるため `CANDIDATE_FONTS` へ含めない。
- `PRG-003`（レイアウト）：狭幅または低画面時は下部詳細タブ用の予約高さを先取りし、上部モニターに上限スクロールを適用する（`crates/agy-gui/src/app.rs`）。
- `PRG-003`（フォント回帰検査）：`CANDIDATE_FONTS` を追加または変更するときは、全実在フォント候補に対してフォールバック登録、行高、垂直中心の回帰検査（`gui_10_all_existing_candidate_fonts_render_properly`）を通す。

## 期間統計とカスタマイズ棚卸しの不変条件

- `PR #5 PRG-001`：`crates/agy-core/src/usage.rs` の `load_or_analyze_session` 等の一括集計では `analyze_conversation_with_meta_and_customizations` へ `Some(CustomizationBreakdown::default())` を明示渡ししてセッションごとの `~/.gemini/` 再走査を防ぎ（単一セッションのリアルタイム監視のみキャッシュ済み内訳を渡す）、`resolve_model_display` でも `model_from_db == false` の `antigravity_state.pbtxt` フォールバック値を計上せず `unknown`（`不明`）扱いとする。
  `crates/agy-core/src/analyzer.rs` で常時読込 5 項目（`mcp_tools`、`mcp_instructions`、`rules_files`、`skills_index`、`custom_agents`）の合計が `fixed_baseline_budget` を超える場合は、遅延読込 3 項目を維持したまま常時 5 項目を予算内へ比例按分し（余りは最大剰余順、同率時は定義順で 1 ずつ配分）、会話実測バケットを押し出さない。
- `Phase 2 棚卸し境界`：`crates/agy-core/src/customizations.rs` の走査対象は `~/.gemini/` 配下の定義済み 5 経路（`antigravity/mcp/`、`antigravity/builtin/skills/`、`antigravity/mcp_config.json`、`config/plugins/`、`config/mcp_config.json`）に限定してシンボリックリンクを除外し、`MAX_SCAN_FILES`、`MAX_FILE_BYTES`、`MAX_SCAN_DEPTH`、`MAX_WARNINGS` による有界上限を緩和または撤廃しない。
  `mcp_config.json` やプラグイン内 `.mcp.json` の認証関連キー（`env`、`headers`、`args`、`token`、`apiKey`、`secret`、`password`、`authorization`）の値はパース直後に破棄して `CustomizationInventory`、UI、ログ、`scan_warnings` へ保持や出力をせず（警告に出せるのは安全なパスとエラー種別のみ）、`~/.gemini/config/config.json` は読み取らない。

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
