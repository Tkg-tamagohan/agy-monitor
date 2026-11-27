# Review Guidelines

この文書は Devin Review 専用のレビュー指針を定める。
設計制約と既知の不変条件は `AGENTS.md` に記載済みであり、Devin Review は両方を併用して判定する。
ここでは `AGENTS.md` の重複を避け、CI の機械検査では拾いにくい不具合と AGY App でのトリアージ効率に絞る。

## レビュー言語

すべての指摘、要約、補足は日本語で出力する。

## 重点チェック領域（Critical Areas）

- **Read-Only SQLite とバイナリ解析**：`crates/agy-core/src/sqlite.rs`、`crates/agy-core/src/proto.rs`、`crates/agy-core/src/models.rs`
- **FS 走査と機密分離、パス解決**：`crates/agy-core/src/customizations.rs`、`crates/agy-core/src/paths.rs`、`crates/agy-core/src/sessions.rs`
- **監視スレッドと並行状態管理**：`crates/agy-core/src/monitor.rs`、`crates/agy-core/src/fingerprint.rs`
- **トークン配分と期間集計**：`crates/agy-core/src/analyzer.rs`、`crates/agy-core/src/usage.rs`
- **GUI スレッド分離と状態同期、描画安全**：`crates/agy-gui/src/app.rs`、`crates/agy-gui/src/usage_view.rs`、`crates/agy-gui/src/fonts.rs`
- **依存関係と CI 条件**：`Cargo.toml`、`crates/*/Cargo.toml`、`.github/workflows/ci.yml`

## 優先して指摘してほしい観点

- **diff 外への波及と既存コードとの干渉**：関数契約や戻り値の意味変化が呼び出し元へ及ぼす影響、バッチ集計経路から `customization_breakdown` に `None` を渡して全ディスク再走査を引き起こす回帰を確認する。
- **境界値と算術安全**：`active_total() == used_tokens` の崩れ、トークン合算のオーバーフローや丸め誤差、ローカル日付の期間境界、プロット座標の `NaN` や負値キャスト、UTF-8 文字境界、手書き Protobuf や YAML パーサの不正入力を確認する。
- **エラーハンドリング漏れ**：非 `CoreError::DbBusy` の SQLite エラー握りつぶし、`~/.gemini` のスキーマ変化への耐性、リトライ待機中のキャンセル遅延を確認する。
- **並行処理と状態管理の不整合**：`agy-monitor-poller` と `agy-monitor-phase2` 間のキャンセルや世代管理、停止順序、キャッシュ失効漏れ、未制限キャッシュやチャネルの肥大化を確認する。
- **GUI の応答性とフィルタ整合**：UI スレッドでのファイルや DB の同期 I/O、毎フレームの不要なコマンド連打、検索文字列とドロップダウン選択の同期崩れを確認する。
- **セキュリティと情報境界**：`conv_id` を経由したパストラバーサル防御、認証値や生 JSON エラー文字列が `CustomizationInventory` や `last_error`、`scan_warnings`、ログへ漏れないこと、`~/.gemini` への書き込みや外部通信が混入しないことを確認する。
- **後方互換性の破壊**：実 `~/.gemini` の SQLite スキーマや transcript JSONL 形式、複数日時形式、コンパクト履歴のマージ条件が壊れないこと、`.github/workflows/ci.yml` の `DOC_ONLY_RE` が Rust 必須チェックを誤スキップしないことを確認する。

## 指摘を抑えてほしい観点・除外対象（Ignore）

### 除外対象ファイル

- `Cargo.lock`（依存変更は `Cargo.toml` と `crates/*/Cargo.toml` で確認し、整合性は CI の `--locked` へ委ねる）

### 指摘を抑える対象

- `cargo fmt`、`cargo clippy -- -D warnings`、`hygiene`（UTF-8 や競合マーカー検査）など CI が機械検知する書式やスタイル、命名の好みなどの主観的指摘
- PR の議論済みスレッドで合意された判断、またはコメントや PR 本文で意図的な設計と明示された箇所への重複指摘
- GitHub の Suggested change 構文（```` ```suggestion ````）の出力（不要とし、修正方針を簡潔な文章で示す）

### 意図的な設計例外

- **ライブ読み取り向けの寛容パース**：`crates/agy-core/src/analyzer.rs` の途中切れ JSONL や lossy UTF-8 スキップ、`crates/agy-core/src/proto.rs` の部分 `ProtoMap` 返却は書き込み中ファイルを読むための仕様とする。
- **オフライン固定と Windows 主対象設計**：`crates/agy-core/src/models.rs` のハードコード済みモデル表、`crates/agy-gui/src/main.rs` の `windows_subsystem`、`crates/agy-gui/src/fonts.rs` の `%WINDIR%\Fonts` 参照、`.github/workflows/ci.yml` の `rust-windows` 限定は意図的な設計とする。
- **GUI とテストの既知制約**：`crates/agy-gui/src/app.rs` の 200 行表示上限やプロット操作無効化、`MainTab::UsagePhase2` の遅延初期化、`crates/agy-gui/src/usage_view.rs` でプロジェクト絞り込みをプロジェクト別テーブルへ限定する仕様、既存の `#[allow(clippy::too_many_arguments)]`、OS スレッド起動の `.expect()` は意図的な設計とする。
<!-- 今後の意図的な設計例外はここへ 1 行ずつ追記する -->

## 指摘の書き方（トリアージ向け）

AGY App 側で誤検知か否かを即座に判定できるよう、各指摘には次の項目を簡潔に含める。

- **対象箇所**：ファイルパスとシンボル名（関数名や構造体名）
- **発生条件と影響**：問題が起きる具体的な入力や状態遷移と、diff 外を含む影響範囲
- **修正方針**：解消条件を 1 文から 2 文で記載する（Suggested change 構文は不要）

コードから確認できない挙動を推測で書く場合は、未確認である旨を明示して確定した不具合と区別する。
