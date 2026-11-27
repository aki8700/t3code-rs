# Orca + Herdr 138-Shard 横断集計・`t3code-rs` 取り込み優先機能リスト

- **分析対象**: `/Users/apple/research/orca-herdr/notes/*.md` 全 138 shard（34,979 ファイル / `check.py` 138/138 通過）
- **抽出総数**: shard 総括 892 件、非空 `真似:` エントリ 26,757 件（詳細ダイジェスト: `/Users/apple/research/orca-herdr/notes/digest-all-138.md`）
- **基準コミット**: `t3code-rs` `main` (`ba7e1d9`)

---

## 0. `t3code-rs` で既に実装済み（対象外）

以下の高頻度テーマはすでに `t3code-rs` 側に同等実装があるため、本リストから除外した。

- Git worktree 分離・差分・マージ・ターン前チェックポイント／ロールバック（`t3-git`）
- PGID + 正規化 `lstart` による孤児プロセス追跡・永続化・未解決時自動再開保留（`resume.rs`）
- 同一ツール失敗の早期停止ウォッチドッグ（2 回警告 / 4 回停止）・コマンド出力末尾保持
- Swarm 待機 wake 統合・`killed_count`・`mode_ceiling` 交差・`Resync` 通知・FTS `rowid` 最適化
- SwiftCUA 同梱・`/gemini` macOS アプリ連携・ゼロテレメトリ・Tailscale / Cloudflare Access 認証
- SQLite WAL + zstd + blake3 重複排除 + FTS5 trigram + `harness-metrics.py`

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

## 2. 推奨実装順（Phase 構成）

1. **Wave 1（P0-2 + P0-3 + P1-1）**: 独立性が高く各 1〜2 ファイルで収まるため、並列ワーカー 3 体で一気に片付く。
2. **Wave 2（P0-1 + P1-2）**: WS プロトコル（`seq`＋3 レーン）と `web/app.js` 末尾描画をセットで刷新し、`verify-t3code` で実ブラウザ検証。
3. **Wave 3（P2-1 + P2-2）**: プロバイダー起動パイプラインの耐障害性強化（`harness-metrics.py` で失敗ラン平均時間・無駄トークン率の改善を比較計測）。
