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

## 3. 推奨実装順（未実装＋既存欠陥の統合 Phase 構成）

1. **Wave 1（S ランク欠陥の止血：G-4 + G-3 + G-5 テスト）**:
   - `Plan` モード時の `swiftcua` 遮断（G-4）、Swarm 配信記録の DB 永続化＋`run_id` 紐付けで「報告なし終了」誤判定根絶（G-3）、`gemini_app.rs` のモック AX ツリー単体テスト追加（G-5）。
2. **Wave 2（並列性能と UI 整合性：P0-2 + P0-3 + P1-1 + G-1 縮小）**:
   - `web/app.js` の `epoch` ガード（P0-2）、リポ単位 Git コアレス（P0-3）、クラッシュループブレーキ（P1-1）、`coordinator.rs` の `active` ロック保持区間から重い DB/Git I/O を外す（G-1）。
3. **Wave 3（配信・描画基盤の刷新：P0-1 / G-2 + P1-2 + G-6 分割）**:
   - WS 3 レーン＋`seq` 再同期（P0-1 / G-2）、`web/app.js` の末尾局所描画（P1-2）とモジュール分割・JS ユニットテスト導入（G-6）。
4. **Wave 4（プロバイダー信頼性仕上げ：P2-1 / G-7 + P2-2 / G-8）**:
   - `await_gate` 起動同期パイプ、初回応答タイムアウト、`resume.rs` の固定 1500ms スリープ短縮（`harness-metrics.py` で改善幅を計測）。
