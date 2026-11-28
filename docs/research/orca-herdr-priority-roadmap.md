# Orca + Herdr 138-Shard 横断集計・`t3code-rs` 取り込み優先機能リスト

- **分析対象**: `/Users/apple/research/orca-herdr/notes/*.md` 全 138 shard（34,979 ファイル / `check.py` 138/138 通過）
- **抽出総数**: shard 総括 892 件、非空 `真似:` エントリ 26,757 件（詳細ダイジェスト: `/Users/apple/research/orca-herdr/notes/digest-all-138.md`）
- **基準コミット**: `t3code-rs` `main` (`ba7e1d9`)

---

## 0. `t3code-rs` に既存実装はあるが、完成度・構造に差がある領域（監査対象）

以前は「既存実装あり」としてスキップした領域にも、Orca / Herdr と比べると**構造的欠陥や完成度の低さ**が複数残っている。未実装リスト（§1）とは別に、§2 に既存機能の弱点・技術的負債を一覧化した。

---

## 1. 優先機能リスト（P0 → P2）

### P0（最優先：信頼性・UI 整合性・負荷の根本改善）

| # | 機能 | 主な参照元（Orca / Herdr） | `t3code-rs` での効き目 | 最小実装スコープ |
|---|---|---|---|---|
| **P0-1** | **境界付き WS イベントハブ＋3 レーン優先度配信（`events_lost → Resync`）** | `herdr` `src/api/event_hub.rs` (`events_after_checked`), `src/server/client_transport.rs` (`ClientWriterQueue`) / `orca` `daemon-stream-backpressure.ts`, `cell-seat-log.ts` | 並列稼働時の大量ストリーム出力で遅いブラウザ／モバイル接続が詰まっても、承認要求や状態遷移などの制御イベントを落とさない。リング溢れ時は欠番検知で自動 `Resync` させ、UI 乖離を構造的に防ぐ。 | `crates/core/src/protocol.rs`（`seq` 付与）, `crates/server/src/ws.rs`（制御／最新置換状態／ドロップ可デルタの 3 レーン＋有界リング）, `web/app.js`（`lastSeq` 欠番時リロード） |
| **P0-2** | **非同期応答の世代フェンス（Generation Fence）と単一終端確定** | `orca` `codex-structured-session-acquire.ts` (`assertCurrent`), `run-completion-watcher.ts` (`finalize`), `github-task-identity.ts` (`claimPublication`), `terminal-output-generation.ts` | スレッド高速切替やキャンセル直後の遅延応答・旧イベントが最新 UI／状態を巻き戻す競合を根絶する。 | `web/app.js`（詳細・Diff・使用量取得に単調 `epoch` ガード導入）, `crates/server/src/coordinator.rs`（run 終端確定時の最新状態再確認） |
| **P0-3** | **リポ単位 Git 更新コアレス（`in_flight` + `due_after_in_flight`）** | `herdr` `src/app/git_refresh.rs` (`deduplicate_git_refresh_items`), `orca` `agent-detection.ts` (`runPreflightCheck`) | 並列ワーカーや頻繁な `swarm_status` / Diff 開閉で同一リポジトリに `git status/diff` が多重起動するのを防ぎ、1 実行中＋完了後 1 回再実行へ束ねる。 | `crates/server/src/threads.rs`（または `crates/git`）にリポパス単位の single-flight コアレス層を追加 |

### P1（次点：長時間自律稼働の安定化・UI 体感速度向上）

| # | 機能 | 主な参照元（Orca / Herdr） | `t3code-rs` での効き目 | 最小実装スコープ |
|---|---|---|---|---|
| **P1-1** | **クラッシュループ・ブレーキ** | `orca` `orcad-managed-crash-loop.ts` (`ManagedOrcadCrashLoopBrake`) | 認証切れ・設定不備等で数秒で落ち続けるプロバイダーの連続自動再試行を短時間 N 回で自動停止し、トークン浪費とログ暴走を防ぐ。 | `crates/server/src/provider_hold.rs` に短時間異常終了ウィンドウ（例: 5 分で 3 回連続失敗 → 自動 hold）を追加 |
| **P1-2** | **末尾局所 DOM 更新（`projection-tail`）＋フレーム間引き** | `orca` `mobile-native-chat-projection-tail.ts` (`projectMobileNativeChatTail`), `write-queue.ts` (`pumpWrites`) | 長大スレッドでのストリーミング出力時にタイムライン全体を再描画せず、末尾要素のみ rAF 単位で更新してブラウザ／モバイルを軽量化。 | `web/app.js` のライブデルタ描画を末尾ノード局所更新＋rAF コアレスに限定 |
| **P1-3** | **既読投影ベースの誤通知抑止＋同一状態イベント間引き** | `herdr` `endpoint_agent_state.rs` (`acknowledge_surface`), `terminal_titles.rs` (`stripped_changed`) | アクティブ表示中スレッドの過剰な OS 通知と、実質変化のないステータス／タイトル連打イベントを発生源で抑制。 | `crates/server` の状態変更 emit 前差分比較、`web/app.js` / `t3-desktop` のフォーカス中通知抑止 |

### P2（発展：エッジケース潰し・耐障害性の完成度引き上げ）

| # | 機能 | 主な参照元（Orca / Herdr） | `t3code-rs` での効き目 | 最小実装スコープ |
|---|---|---|---|---|
| **P2-1** | **起動前同期ゲート（`await_gate`）によるプロセス追跡窓ゼロ化** | `orca` `native/provider-exec/provider-exec.c` (`await_gate`) | `spawn()` 直後〜SQLite `run_orphan_processes` 書き込み完了前のクラッシュ窓を塞ぐ（親が DB 記録後にパイプを閉じるまで子 CLI を開始しない）。 | `crates/providers/src/process.rs` の Unix `pre_exec` 同期パイプ＋ `coordinator.rs` の DB 記録後リリース |
| **P2-2** | **プロンプト投入後の初期無反応検知（二段階反応検証）** | `herdr` `src/api/wait.rs` (`prompt_agent`), `orca` `terminal-pty-readiness.ts` | 起動直後にプロンプトを受理できず無音ハングする CLI を、900 秒待たずに短い初期起動タイムアウト（例: 30〜60 秒）で早期失敗・フォールバックさせる。 | `crates/providers/src/process.rs` に初回イベント到着までの起動期限（startup timeout）を追加 |
| **P2-3** | **構造保持・symlink 安全な外部設定ファイル編集ユーティリティ** | `herdr` `src/integration/claude_settings.rs`, `config_file.rs` | `setup-agy-mcp` や将来の外部 CLI 連携で既存設定を壊さず、symlink / hardlink 経由の意図しない書き換えを防ぐ。 | `crates/cli` の設定更新ヘルパーを強化 |

---

## 2. 既存機能の完成度ギャップ・構造的欠陥・低完成度リスト

「機能自体は存在するが、Orca / Herdr 水準と比べて構造的な弱さやプロトタイプ段階の粗さが残っている箇所」を深刻度順に整理した（2026-11-27 実コード監査ベース）。

### S（構造的欠陥：スケール・並列稼働・安全境界に直結）

| # | 領域・箇所 | 現状の構造的欠陥 / 完成度ギャップ | Orca / Herdr との差 |
|---|---|---|---|
| **G-1** | **全スレッド共通の単一 `active` ロック**<br>`crates/server/src/coordinator.rs`（14箇所）, `swarm.rs` | 全スレッド共有の 1 つの `tokio::sync::Mutex<HashMap<Uuid, ActiveRun>>` を握ったまま SQLite I/O・Git・worktree 作成・中断待機を実行。1 スレッドの遅延が無関係な全スレッドの開始・完了・キャンセルをブロックする。 | Herdr/Orca はスレッド／セッション／リポ単位のロックまたはアクター分離で、他スレッドを止めない。 |
| **G-2** | **単一 `broadcast(512)` 頼みの WS 配信**<br>`crates/server/src/coordinator.rs:186`, `ws.rs:27-93` | 高頻度デルタと重要な制御イベント（承認要求・状態遷移）が同じ 512 枠の broadcast に混在。遅い回線では `socket.send().await` で詰まって `Lagged` → 毎回全再取得になる。`Subscribe`〜初回 `GET` 間の取りこぼしを防ぐ `seq` 再同期もない。 | Herdr は制御／最新置換状態／ドロップ可ストリームの 3 レーン＋`events_after_checked(last_seq)`。 |
| **G-3** | **Swarm 配信状態のインメモリ揮発 & 時刻ズレ誤判定**<br>`crates/server/src/swarm.rs:26, 738-760` | `WakeDeliveries` がメモリ上の `Mutex` のみで、`t3d` 再起動時に配信範囲・`redeliveries`・`wake_depth`・`mode_ceiling` の対応が消える。また「報告なし終了」判定が `run_started_at` との時刻比較＋200 件ずつの全ページ走査に依存しており、実際に `#179` 報告直後に `#180` 誤判定が起きている。 | Orca は DB 永続の run 世代・終端確定（`finalize`）で、再起動や時刻ズレで誤判定しない。 |
| **G-4** | **`SwiftCUA` 統合の安全境界が未接続**<br>`crates/providers/src/cli_drivers.rs` | `computer_use_enabled = true` で `cua mcp` を無条件に注入するだけで、`InteractionMode::Plan` 時にも `click` / `type_text` 等の書き込み系 GUI ツールが露出する。`t3d` 側のアプリ許可リスト・承認リレー連携も未接続。 | Codex / SwiftCUA 本来の「セッション単位アプリ許可＋承認ゲート」を `t3d` のモード上限と接続できていない。 |

### A（完成度が低い・プロトタイプ水準・保守限界が近い）

| # | 領域・箇所 | 現状の弱点・完成度の低さ | Orca / Herdr との差 |
|---|---|---|---|
| **G-5** | **`/gemini` マックアプリ連携（プロトタイプ段階）**<br>`crates/server/src/gemini_app.rs`（655行） | ① `static SESSION: Mutex<()>` で全体 1 並列固定、② 完了検知が `sleep(1s)`＋日本語 UI ラベル／`copy-button` 依存で脆弱（現に長文 `AXTextArea` 欠落を踏んだばかり）、③ 質問 16KB 固定上限・ファイル添付不可、④ **ユニットテスト 0 件**。 | テスト可能な抽象層・構造化パーサ・リトライ／中断処理が不足。 |
| **G-6** | **`web/app.js` 5,347 行の単一巨大ファイル**<br>`web/app.js` | UI 状態・`fetch`・DOM 描画・WS ハンドラが 1 ファイル 5,347 行に集中。スレッド切替時の非同期応答レース防止（`epoch` ガード）が局所的で、長大スレッドの描画最適化も不十分。JS 単体テストはゼロ（`node --check` と 1 本のスモークのみ）。 | Orca は純粋 reducer＋`claimPublication` 世代ガード＋`projection-tail`＋単体テストで分離。 |
| **G-7** | **孤児回収の起動窓と `ps` テキストパース依存**<br>`crates/server/src/resume.rs`, `process.rs:467-475` | `cmd.spawn()`〜SQLite 記録完了前のクラッシュ窓が残る。毎回外部 `ps -A` を起動して文字列パースしており、`SIGTERM` 後もプロセスが即終了しているのに固定 `1500ms` スリープする。 | Orca は `await_gate` パイプで起動前 DB 記録を保証し、終了を即座に検知する。 |
| **G-8** | **プロバイダー起動無反応＆ハング時の高コスト損失**<br>`crates/providers/src/process.rs`（2,202行） | 起動直後の無反応検知がなく 900 秒のアイドルタイムアウトまで待つ。実測 `state.sqlite` でも失敗ラン平均 400〜487 秒、Codex 中断ラン平均 1,063 秒（Codex 全トークンの 29% を浪費）。 | Herdr は投入後 5 秒の反応検証（`prompt_agent`）で無音ハングを即検知する。 |

### B（技術的負債・将来の拡張ボトルネック）

| # | 領域・箇所 | 現状の課題 |
|---|---|---|
| **G-9** | **巨大ファイルへの責務集中**<br>`coordinator.rs`（2,714行）, `limits.rs`（2,330行）, `process.rs`（2,202行）, `usage.rs`（1,943行）, `store/lib.rs`（1,901行）, `threads.rs`（1,821行） | オーケストレーション・クォータ外部仕様パース・ストリーム解析・DB 操作が少数ファイルに肥大化しており、変更時の影響範囲が広くリグレッションが起きやすい。 |
| **G-10** | **時間依存のフレーキーテスト＆リポ非ルート時の worktree 制約**<br>`crates/server/tests/http.rs`, `crates/providers/src/process.rs`, `crates/git/src/lib.rs` | CPU 高負荷時に `cloudflare_rs256_*` や `cancelled_approval_*` がタイミング切れで稀に失敗する。また、プロジェクトルートが git リポの親ディレクトリの場合（複数リポ配下）、`isolation: "worktree"` が自動で解決できない。 |

---

## 3. トークン節約・ハーネス最適化リスト（実測データ＋Orca 由来）

`state.sqlite` 実測（完了ラン平均）では **coordinator が 119 万トークン/回（平均プロンプト 2,959B）・worker が 517 万トークン/回**、さらに **Codex のキャッシュ率は 61%（Claude は 97.5%）**・**Codex のキャンセル損失が全体の 29%** に達している。ここを直接削るハーネス改善を優先度順に整理した。

| # | 施策 | 根拠・参照元 | 期待効果 | 最小実装スコープ |
|---|---|---|---|---|
| **T-1** | **再開セッション時の Swarm ボイラープレート省略** | `crates/server/src/swarm.rs:74-95`。現状は既存セッション再開でも毎ターン約 1.3KB の `Swarm board ... T3_SWARM_ACTION ...` 全文を再注入している。 | coordinator 履歴の肥大化とキャッシュ後半の揺れを削減（毎ターン 400〜600 トークン節約 × 数百ターン）。 | `swarm.rs`: `resume_session_id.is_some()` 時は指示全文を省き、変更があった `Members:` 差分のみ付与。 |
| **T-2** | **役割別 MCP ツールプロファイル（最小ツール露出）** | `crates/mcp/src/lib.rs`, `cli_drivers.rs`。全ロールに 22 ツール＋`swiftcua` 17 ツール＝計 39 ツール定義が毎ターン乗る。 | worker から `swarm_add/kill`・`swiftcua`・`thread_fork` 等を外すだけで毎ターン数千トークンの固定入力を削減（同時に誤操作防止・G-4 解消）。 | `McpInjectionSpec` にロール（`coordinator` / `worker` / `user`）と `computer_use` 許可フラグを持たせ、`tools/list` と `swiftcua` 注入を絞る。 |
| **T-3** | **`compaction_econ` 連動の自動コンパクション／自動ハンドオフ** | `crates/maki/src/econ.rs`, `crates/server/src/compact.rs`。経済計算器と圧縮実装はあるが手動トリガーのみで自動発動しない。 | 長期 coordinator（平均 119 万トークン/回）と大型 worker の入力トークン膨張を自動で頭打ちにする。 | `coordinator.rs`: コンテキスト 75% 超かつ `compaction_econ` が黒字判定のターン境界で自動コンパクト（非対応プロバイダーは自動ハンドオフ）。 |
| **T-4** | **コンテキスト項目別＋全体予算キャップ＆`t3-maki` 標準利用** | `orca` `linear-issue-context-snapshot.ts` (`LINEAR_ISSUE_CONTEXT_CAPS`), `journal-domain-schema-001-outline.ts`（ハッシュ＋予算キャッシュ） | handoff 要約・verify 失敗ログ・ファイル読み込みの無制限膨張を項目別上限＋`[truncated]` で抑え、`code_index`/`code_slice` 優先で worker の 517 万トークン/回を大幅圧縮。 | `crates/server/src/threads.rs`（handoff の項目別バイト上限）、`crates/maki`（内容ハッシュキャッシュ）、`swarm.rs`（worker 規約に `code_index/slice` 優先を明記）。 |
| **T-5** | **ワーカーのトークン／ツール回数予算ガード＆早期打ち切り** | `state.sqlite` 実測（Codex 中断 16 回で 8,000 万トークン・17.7 分/回を喪失）。 | 迷走したワーカーが枠を焼き切る前に「N ツール呼出 or M トークン超過で中間報告を強制」＋起動無反応（60 秒）で即切り上げ。 | `crates/providers/src/watchdog.rs` に soft-budget 警告（steering 注入）と hard-budget 停止を追加。 |
| **T-6** | **Codex / Antigravity 向けプロンプトキャッシュ・プレフィックス固定化** | 実測キャッシュ率: Claude 97.5% vs Codex 61%。`orca` `moduleCacheIdentity` / `cachePrefix`。 | システム指示・ツール定義・プロジェクト指示を先頭固定し、可変情報（現在時刻・メンバー一覧・未読件数）を完全末尾化して Codex キャッシュ率を 85%+ へ引き上げる。 | `crates/providers/src/cli_drivers.rs`, `swarm.rs`, `coordinator.rs` にプレフィックス不変回帰テストを追加。 |
| **T-7** | **実測駆動のモデルルーティング＋`/gemini` 無料枠活用** | `scripts/harness-metrics.py` + `gemini_app.rs`。 | 探索・定型読込＝`Tier1Fast`（Haiku/Hydrogen）、設計・レビュー＝`/gemini`（API 0 トークン）、実装＝Codex/Haiku の自動推奨を `orchestrator_capabilities` に載せ、最適化ループを `harness-metrics.py` の前後比較で検証。 | `crates/mcp` の `orchestrator_capabilities` に推奨ルーティングを返し、`scripts/harness-metrics.py` で週次改善を追跡。 |

---

## 4. 推奨実装順（未実装＋既存欠陥＋トークン節約の統合 Phase 構成）

1. **Wave 1（即効性・トークン節約＆S ランク止血：T-1 + T-2/G-4 + G-3）**:
   - 再開時 Swarm ボイラープレート省略（T-1）、役割別 MCP ツール絞り込み＋`Plan` 時 `swiftcua` 遮断（T-2/G-4）、Swarm 配信永続化＆`run_id` 判定（G-3）。
2. **Wave 2（トークン暴走防止＆並列性能：T-3 + T-5/G-8 + P0-2 + P0-3 + P1-1）**:
   - `compaction_econ` 自動コンパクション（T-3）、予算ガード＆初期無反応タイムアウト（T-5/G-8）、`web/app.js` 世代フェンス（P0-2）、リポ単位 Git コアレス（P0-3）、クラッシュループブレーキ（P1-1）。
3. **Wave 3（配信・描画基盤の刷新：P0-1/G-2 + P1-2 + G-1/G-6）**:
   - WS 3 レーン＋`seq` 再同期（P0-1/G-2）、`web/app.js` 末尾局所描画＆分割（P1-2/G-6）、`coordinator.rs` の `active` ロック縮小（G-1）。
4. **Wave 4（耐障害性と最適化仕上げ：T-4 + T-6 + P2-1/G-7 + G-5 テスト）**:
   - `t3-maki` ハッシュキャッシュ＋handoff 項目別キャップ（T-4）、プレフィックス不変テスト（T-6）、`await_gate` 起動同期（P2-1/G-7）、`gemini_app.rs` 単体テスト（G-5）。

---

## 5. 実装反映・第 2 弾ハーネス改善ログ（2026-11-27 完了）

- **Waves 1〜4 反映済み（Gemini Mac アプリ + Muse アプリ）**:
  - **T-1**（再開時 Swarm 定型文省略）、**T-2 / G-4**（役割別 MCP ツール絞り込み 22→8 ツール＋`Plan`/`Worker` 時 `swiftcua` 遮断）、**T-3**（`compaction_econ` 連動 `should_auto_compact`）、**T-4**（`t3-maki` コンテンツハッシュキャッシュ＋`cap_section_utf8`）、**T-5 / G-8**（`TurnBudgetWatchdog`＋初期無反応 60 秒打ち切り）、**T-6**（プレフィックス不変＋再開決定性テスト）、**T-7**（`orchestrator_capabilities` 推奨ルーティング）。
  - **G-3**（`WakeDeliveries` SQLite 永続化＋`sender_run_id` 無報告終了判定）、**G-5**（`gemini_app.rs` `code-widget-*` 抽出修正＋単体テスト 4 件）、**G-7**（`resume.rs` 50ms 高速 SIGTERM ポーリング）。
  - **P0-1 / G-2**（WS 3 レーン優先度キュー `ClientLaneQueue`＋`seq` エンベロープ）、**P0-2 / P1-2 / P1-3 / G-6**（Muse 実装：`web/js/ui-guards.js` 世代フェンス・rAF 末尾局所描画・通知ゲート＋`node --test` 17 件）、**P0-3**（`RepoCoalescer` を `GitManager::diff_summary` へ接続）、**P1-1**（クラッシュループブレーキ `record_failure` / `record_success`）。
- **第 2 弾トークン＆ハーネス改善（`harness-metrics.py` 1,125 ラン実測ベース：`swarm:worker` 50.7% 浪費対策）**:
  - **R2-1**: `TurnBudgetWatchdog::for_profile(McpToolProfile)` によりワーカー専用タイト予算（soft 25 / hard 50 tools、soft 2.5M / hard 5M tokens）を自動適用。
  - **R2-2**: ワーカー初回規約に `code_index` / `code_slice` / `code_grep` 優先を明記。
  - **R2-3**: `handoff_summary` へセクション別（2.5KB）・全体（8KB）UTF-8 キャップ（`[truncated]`）を適用。
  - **R2-4**: `Cancelled` / `RateLimited` / `Failed` 終了時に直近の部分進捗（400 文字・`@` 無害化・4096B 上限）を `@coordinator` へサルベージ通知。
- **Muse Wave 2 反映済み（`955bd9d`）**:
  - `web/js/ui-guards.js` に `createSeqTracker` を追加し、`web/app.js` へ `events_lost` / `seq` 欠番検知と自動再同期を配線。
  - `web/app.js` から純粋関数 19 件を `web/js/thread-helpers.js` へ分割し、`web/js/thread-helpers.test.mjs` を追加（`node --test` 計 71 件パス）。
- **第 3 弾ハーネス改善反映済み（Gemini Mac アプリ）**:
  - **R2-5**: `opencode_args` に `--print-logs --log-level ERROR` を付与し、`process.rs` の `read_tail_watching_limit` が `Free usage limit reached` を stderr から 1 秒未満で検知・即 `RateLimited` 終了するよう改善（60 秒無反応待ちを解消）。
  - **R2-6**: `SwarmService::add_worker_locked` に `ProviderHolds` 事前ゲートを追加し、hold 中モデルでの空スレッド／ワークツリー作成を防止。
  - **P1-3**: `Coordinator` に `ThreadStatusDeduper` を組み込み、同一 `ThreadStatusChanged` の連続 WS 送出を抑制。
  - **P2-3**: `crates/cli/src/main.rs` に symlink 拒否＋`0o600` 原子的設定書き込み `write_config_safely` と単体テストを追加。
- **夜間 Codex（`gpt-6.2-astra` / `xhigh`）Wave 1 反映済み（2026-11-28 01:09〜01:22 JST）**:
  - **G-9（`usage.rs` 分割）**: `crates/server/src/usage.rs`（1,943 行）を `crates/server/src/usage/` 配下の 9 モジュールへ分割（全ファイル 587 行以下、API・テスト完全維持、`7b321b7`）。
  - **G-9（`limits.rs` 分割）**: `crates/server/src/limits.rs`（2,330 行）を `crates/server/src/limits/` 配下の 7 モジュールへ分割（全ファイル 577 行以下、API・テスト完全維持、`f8cc44e`）。
  - **G-10（非ルート git 解決・WS 3 レーン試験・HTTP フレーキー対策）**: `crates/git/src/lib.rs`、`crates/server/src/ws.rs`、`crates/server/tests/http.rs` を強化（`0dfac4e`）。
  - **P2-1（Unix `pre_exec` `await_gate` 起動同期パイプ）**: `crates/providers/src/process.rs` / `lib.rs`・`crates/server/src/coordinator.rs` へ `spawn()`〜SQLite 孤児記録完了前のクラッシュ窓を塞ぐ同期ゲートを実装・単体テスト追加。
- **保留／未着手**:
  - **DeepSeek**（`opencode/deepseek-v4.1-flash-free`）は OpenCode 無料枠上限（`AI_APICallError: Free usage limit reached`）のため、該当実装はすべて Gemini Mac アプリへフォールバック済み。
  - **未着手（大改修枠）**: **G-1**（`coordinator.rs` `active` ロック縮小）、**G-9 残り**（`coordinator.rs` / `process.rs` / `threads.rs` / `store/lib.rs` 分割）。
- **2026-11-28 残りの実装（Codex + Claude 直接修正）反映済み**:
  - **G-9 完了**: `threads.rs`（0fff3c5）、`store/lib.rs`（9ac2696）、`process.rs`（fa88e2e）、`coordinator.rs`（6104baf、2,976 行 → 506 行 + `coordinator/{execute,queue,start,state,stream}.rs`。`execute` 本体 886 行は分割せず 890 行の 1 ファイル）。
  - **G-1 完了**（5ba6c7c）: 全スレッド共通の `active` ロック保持を撤廃し、スレッド単位（`KeyedLocks`）・プロジェクト単位・ボード単位のロックへ。`active` は `std::sync::Mutex`。Gemini レビューで見つかった「削除済みスレッドで run が始まる」競合ほか A〜E を修正。
  - **F 完了**: swarm ワーカー上限撤廃（F-1）、`gemini-app` ワーカー（F-2、343967b）、Gemini ブリッジ安定化（F-3 + 実機回帰で 1/12 → 12/12、113f3c3・1169ed3・98baf34）、利用枠切れからの自動再開（F-4、7cced84）。
  - **ハーネス**: ターン予算を 5 倍（0ad8cde）、キャッシュ読みを予算から除外（a5bc07f）。
  - **方針変更**: Gemini の画面操作自動化はやめ、Google Drive（T3-Gemini）＋ Gemini のスキル／スケジュールでの委譲に移行。Antigravity（agy）は規約上、自動化の経路から撤去。
