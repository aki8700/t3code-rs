# agent harness の verify/retry loop と再起動後の再開

調査日 2026-11-26。一次ソース（公式 docs、各 repository の `main` のソースコード）だけを使った。
ソースコードの URL は `main` を指すため、内容は変わりうる。

## 出典と要点

### Claude Code

| 出典 | 要点 |
| --- | --- |
| https://code.claude.com/docs/en/hooks.md | Stop hook は `decision: "block"` で turn を続けさせる。連続 8 回で打ち切り（tool call があれば数え直し）、`stop_hook_active` で再入を判定する。 |
| https://code.claude.com/docs/en/hooks-guide.md | Stop hook は user の interrupt では発火せず、API error は別 event の `StopFailure` になる。 |
| https://code.claude.com/docs/en/sessions.md | `--resume` / `--continue` は transcript を読み戻す。前の process の終了時に実行中だった tool は再実行しない。同じ session を 2 process で開くと片方の message が欠けうる。 |
| https://code.claude.com/docs/en/checkpointing.md | checkpoint は turn を始める prompt ごとに「その前の」コードを保存し、resume 後も `/rewind` できる。Bash や外部の変更は追跡しない。 |

### OpenAI Codex CLI (codex-rs)

| 出典 | 要点 |
| --- | --- |
| https://github.com/openai/codex/blob/main/codex-rs/core/src/responses_retry.rs | stream error は上限付き retry。server の retry-after に従い、上限後は transport を切り替えるか error を返す。budget 超過などは retry しない。 |
| https://github.com/openai/codex/blob/main/codex-rs/model-provider-info/src/lib.rs | 既定は stream retry 5 回、request retry 4 回。 |
| https://github.com/openai/codex/blob/main/codex-rs/exec/src/lib.rs | `codex exec resume` は thread id か `--last` を解決して `thread/resume` を呼ぶ。 |
| https://github.com/openai/codex/blob/main/codex-rs/rollout/src/writer_lock.rs | thread ごとの file lock で writer を 1 つに限る（2 つ目は `already has an active writer`）。 |
| https://github.com/openai/codex/blob/main/codex-rs/rollout/src/recorder.rs | 追記を再開する前に末尾の改行を補う。読めない行は数えて飛ばす。 |
| https://github.com/openai/codex/blob/main/codex-rs/core/src/context_manager/normalize.rs | 復元した履歴に出力のない tool call があれば `aborted` の出力を合成する。 |

### OpenHands (software-agent-sdk)

| 出典 | 要点 |
| --- | --- |
| https://docs.openhands.dev/sdk/guides/agent-stuck-detector | 同じ action と observation の反復、同じ error の反復、独り言、交互反復を検出して止める。 |
| https://github.com/OpenHands/software-agent-sdk/blob/main/openhands-sdk/openhands/sdk/conversation/stuck_detector.py | 比較は id などを除いた内容で行う。action-error は停止前に 1 回 nudge する。resume 後は判定窓を切り直し、直後の再 STUCK を防ぐ。 |
| https://github.com/OpenHands/software-agent-sdk/blob/main/openhands-sdk/openhands/sdk/conversation/types.py | 閾値の既定は action-observation 4、action-error 3、monologue 3、alternating 6。 |
| https://docs.openhands.dev/sdk/guides/convo-persistence | `base_state.json` と event 1 件 1 file の log から、同じ conversation id で復元する。 |
| https://github.com/OpenHands/software-agent-sdk/blob/main/openhands-sdk/openhands/sdk/conversation/state.py | 対応する observation のない action を列挙できる。再起動後は error event を当てて閉じる前提。 |

### SWE-agent

| 出典 | 要点 |
| --- | --- |
| https://github.com/SWE-agent/SWE-agent/blob/main/sweagent/agent/reviewer.py | retry loop は `max_attempts`、`cost_limit`、`max_accepts`、残り予算の下限で打ち切り、全試行から最良を選ぶ。 |
| https://github.com/SWE-agent/SWE-agent/blob/main/sweagent/agent/agents.py | 形式違反などの再問い合わせは `max_requeries`（既定 3）まで。 |

### Aider

| 出典 | 要点 |
| --- | --- |
| https://github.com/Aider-AI/aider/blob/main/aider/coders/base_coder.py | lint / test の失敗を次の message として戻す reflection は `max_reflections = 3` まで。反復内容の判定はこの loop にはない。 |

## t3code-rs の現状との比較（`5ffbaad` 時点）

| 観点 | 他の harness | t3code-rs |
| --- | --- | --- |
| verify 修正 loop の上限 | Aider 3、Claude Code Stop hook 連続 8 | あり。`verify_max_auto_fixes` 既定 2、上限 10 |
| 失敗の反復で打ち切り | OpenHands は直近の同一 pair を数える | あり。過去の全 signature と比較し、A→B→A も止まる。ただし `cargo test` の出力では一致しなかった（差分 2） |
| 失敗履歴の引き継ぎ | SWE-agent は試行を蓄積して比較 | あり。`DEAD ENDS` を修正 prompt に入れる（直近 10 件） |
| turn 内の tool 反復 | OpenHands StuckDetector | 部分的。`watchdog.rs` が同一 tool 呼び出し 8 連続を検出する。出力は比較せず、交互反復は未検出 |
| rate limit の retry | Codex は stream 5 / request 4 | あり。capacity だけ最大 2 回。usage limit は fallback model へ |
| 再起動後の再開回数 | （比較対象に明示の上限なし） | あり。run ごとに 1 回（`restart_resumes`） |
| 再開時の prompt | Claude Code は中断した tool を再実行しない | あり。元の prompt を送らず、状態確認から続ける固定文 |
| 二重 writer の防止 | Codex は thread lock | あり。daemon lock と、残った provider process group の停止 |
| 再開前の session 確認 | Codex は id を解決してから resume | あり。session file を確認し、使えなければ cancel |
| turn 前 checkpoint | Claude Code は prompt の前の状態を保持 | 再開時に上書きしていた（差分 1） |

## 今回埋めた差分

### 1. 再開した run が pre-run checkpoint を上書きする

`execute` は開始のたびに worktree の tree を `save_run_checkpoint` に保存し、同じ run id なら上書きしていた。
再開した run の snapshot は中断した turn の編集を含むため、次の 2 つが起きた。

- 再開後の turn が何も編集しなければ `tree_changed` が偽になり、`verify_command` を実行せずに `Completed` になる。
- その run の rollback が、run の前ではなく中断時点に戻る。

最初の保存を正とし、`save_run_checkpoint` が baseline を返して coordinator がそれを使う。schema は変えていない。
再現 test は `resumed_run_verifies_and_rolls_back_edits_made_before_the_restart`。

選んだ理由: 検証を通らずに完了扱いになり、利用者の rollback も壊れる。修正は数十行で済む。

### 2. 同じ失敗の signature が実行のたびに変わる

rustc 1.100.0 の test binary を 2 回実行すると `thread 'a' (28490) panicked` と `(28497)` になった（実測）。
signature はこの id を残していたため、panic で失敗する `cargo test` では "same result twice" が成立せず、上限まで修正 run を使っていた。
並列 test の完了順と、4,000 文字の tail に残る成功行の違いでも同じことが起きる。

signature の計算で thread id を除き、末尾が ` ... ok` の行を捨て、行を sort する。修正 prompt に入れる出力は変えていない。
代償として、行の順序だけが違う 2 つの失敗は同じとみなす。

選んだ理由: この repository 自身の verify に使う command で打ち切りが働かず、修正は `verify.rs` 内で閉じる。

## 残した差分（未実装）

| 差分 | 内容 | 見送った理由 |
| --- | --- | --- |
| 完了と follow-up の登録が別 transaction | `finish_run` の後、`enqueue_with_meta` の前に落ちると、verify の修正 run や fallback run が失われる | 窓が短い。直すには `store/src/lib.rs` の `finish_run` を含めて 1 transaction にする（schema 変更は不要） |
| 正常終了の再起動は再開しない | SIGTERM では `interrupt_all` が run を `Cancelled` にするため、再開の対象は異常終了だけ（`lib.rs` と `coordinator.rs` の読解による。実機では未確認） | 利用者の interrupt と区別する仕様判断が要る |
| 出力まで見る stuck 判定、交互反復 | OpenHands の action-observation / alternating に相当 | `crates/providers` の担当範囲で、誤検知の計測が要る |
| 停止前の nudge | OpenHands は同じ error の反復で 1 回指示を挟む | verify loop は `DEAD ENDS` を毎回渡しており効果が重なる |
| 費用の予算 | SWE-agent は `cost_limit` で打ち切る | 試行回数の上限があり、費用の計上は provider ごとに欠ける |
