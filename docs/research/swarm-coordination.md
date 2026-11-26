# Swarm coordination: 一次資料と t3code-rs の差分

調査日: 2026-11-26。対象コミット: 5ffbaad。各行の要点は取得した本文に基づく要約です。

## 資料

| 資料 | 1行の要点 |
|---|---|
| [Anthropic: How we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system) | brief には目的、出力形式、使う道具と情報源、担当範囲を書き、手間は課題の複雑さに合わせる（事実確認は1体で3〜10 tool call など）。 |
| 同上（付録） | worker は成果物を外部（ファイルなど）に保存し、lead へは軽い参照だけを返すと、伝言による劣化と token を減らせる。 |
| 同上（費用） | multi-agent は chat の約15倍の token を使うと報告している。 |
| [Anthropic: Building effective agents](https://www.anthropic.com/engineering/building-effective-agents) | 単純な構成から始め、agent には最大反復回数などの停止条件を置く。 |
| [OpenAI Agents SDK: Handoffs](https://openai.github.io/openai-agents-python/handoffs/) | handoff の引数は schema で型付けでき、次の agent に渡す履歴は `input_filter` で絞れる。 |
| [OpenAI Agents SDK: Guardrails](https://openai.github.io/openai-agents-python/guardrails/) | 出力や tool 呼び出しの検査が失敗すると tripwire で実行を止める。 |
| [OpenAI Agents SDK: Running agents](https://openai.github.io/openai-agents-python/running_agents/) | `max_turns` を超えると `MaxTurnsExceeded` になり、error handler で制御された最終出力に置き換えられる。 |
| [LangGraph supervisor](https://github.com/langchain-ai/langgraph-supervisor-py) | worker の結果は全履歴（`full_history`）か最後の応答（`last_message`）かを選んで supervisor に戻す。 |
| [LangGraph swarm](https://github.com/langchain-ai/langgraph-swarm-py) | 中央の lead を置かず、handoff tool で active agent を切り替える。既定の handoff は全メッセージ履歴を渡す。 |
| [Magentic-One 論文](https://arxiv.org/html/2411.04468v1) | 外側ループの task ledger（事実・推測・計画）と、内側ループの progress ledger（完了、loop、進捗、次の担当、指示）の2層で進める。 |
| [Magentic-One orchestrator 実装](https://github.com/microsoft/autogen/blob/main/python/packages/autogen-agentchat/src/autogen_agentchat/teams/_group_chat/_magentic_one/_magentic_one_orchestrator.py) | 進捗なしか loop なら stall を加算し、進捗があれば減算する。`max_stalls`（既定3）に達すると再計画し、`max_turns`（既定20）を超えると最終回答を作って終了する。 |
| [Claude Code: Subagents](https://code.claude.com/docs/en/sub-agents) | subagent は要約だけを親に返し、`maxTurns` で止まった出力には partial の印が付く。 |
| [Claude Code: Agent teams](https://code.claude.com/docs/en/agent-teams) | teammate が停止すると最終回答付きで lead に通知し、API error なら失敗とエラー本文を通知する。`TeammateIdle` / `TaskCompleted` hook で差し戻せる。 |

取得方法: `rx-search` と `r.jina.ai`。arXiv は `r.jina.ai` 側で一時的に拒否されたため直接取得した。

## t3code-rs today vs them

「today」は 5ffbaad の状態、「このブランチ」は `swarm/swarm-coordination` での変更です。

| 観点 | 先行例 | t3code-rs today | このブランチ |
|---|---|---|---|
| worker brief | 目的、出力形式、道具、境界、手間の目安 | 空でない自由文（4096バイトまで） | coordinator への指示と tool description に objective / output / boundaries / effort budget / stop condition を明記（強制はしない） |
| 結果の圧縮 | 要約と参照を返す | `[done]` で始める自由文。上限超過は拒否のみ | `result:` / `verified:` / `blockers:` / `files:` を1投稿で返し、詳細はファイルにするよう指示。上限超過の error にも対処を記載 |
| 停止の通知 | idle / 失敗を lead に通知 | 投稿せず終了した worker を coordinator に通知（実装済み） | 変更なし |
| stall / loop | `max_stalls` で再計画 | verify の同一失敗検出はあるが swarm にはない。無言終了通知は coordinator の wake depth（idle なら0）を使い、再試行ループが上限を迂回する | 終了した worker の depth を引き継ぎ、同じ上限8で停止。coordinator には同じ依頼の再送を避けるよう指示 |
| 終了条件 | 上限到達は明示的な例外や partial 表示 | `MAX_WAKE_DEPTH` 到達後の mention は無言で捨てる（`woken: []` のみ） | 起こさなかった member を `system` 投稿で board に表示 |
| guardrail | tripwire で停止 | runtime / interaction mode の権限上限、active worker 8体 | 変更なし |
| token / effort 予算 | 複雑さに応じた予算 | worker ごとの provider / model / effort 指定のみ | brief に effort budget を書く指示のみ。強制的な予算は未実装 |

## 現場の観測（`~/.t3-rs/state.sqlite`、読み取り専用、2026-11-26）

- board 1件、worker 42体、投稿96件。worker の投稿47件のうち46件が `[done]` を含む。
- 無言終了の `system` 通知は1件、action 失敗通知は0件。同じ本文の重複投稿は0件。
- `scout-forks` は完了報告を `(1/4)`〜`(4/4)` に分割して約12秒で投稿した。各投稿が `@coordinator` を mention している。
- 無限ループと depth 上限での取りこぼしは、この DB では確認していない。前者は fixture driver の test で再現した（修正前は15秒で provider 起動462回）。

## 選んだ2件と理由

1. **停止条件を実際に効かせ、見えるようにする。** 上限の迂回は token を際限なく消費する欠陥で、修正は `swarm.rs` 内の depth の引き継ぎと通知1件で済みます。
2. **brief と done report の契約。** Anthropic が失敗の主因に挙げる箇所で、分割報告という実例もあります。指示文と error message の変更だけで済みます。

## 見送ったもの

- **Magentic-One 型の stall counter / progress ledger:** 「進捗の有無」の判定に毎 turn の LLM 評価が必要です。観測した無言終了は1件で、まず wake depth の上限で足ります。
- **done report の field を server で必須にする:** 拒否された報告は action 行経由だと本文が board に残りません。指示だけで守られない例が出てから検討します。
- **worker ごとの token 予算:** 予算の保存に schema 変更が必要で、停止処理は今回の担当外ファイル（`coordinator.rs`）に入ります。

## 残る制限

- wake depth は連鎖の長さの上限で、進捗は見ていません。正常な長い連鎖も8段で止まり、user の投稿で再開します。
- coordinator が実行中に user が投稿すると、coordinator の depth が使われる挙動は変えていません。
- 再起動をまたぐ depth の扱いは確認していません。
