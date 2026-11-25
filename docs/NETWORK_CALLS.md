# t3code-rs のネットワーク通信

`t3d serve` が自ら送る外部 HTTP リクエストは、Cloudflare Access 認証を有効にしたときの JWKS 取得と、`antigravity_quota_probe = true` を明示有効化したときの Antigravity クォータ照会だけです。製品内にテレメトリ、利用分析、クラッシュ報告、更新確認、Clerk、T3 Connect リレーへの送信処理はありません。

| 発信元 | 宛先と契機 | 送信内容 |
| --- | --- | --- |
| `t3d serve` の認証 | `cloudflare-access` モードで RS256 JWT の署名鍵がキャッシュにないか期限切れの場合、`GET https://<team-domain>/cdn-cgi/access/certs`。設定した `jwks_url_override` があればその URL へ送信します。上書き先は HTTPS または許可されたループバック HTTP です。 | 公開署名鍵の取得リクエスト。検証する JWT、認証 Cookie、プロンプトはリクエストに含めません。リダイレクトと環境プロキシは無効です。 |
| `t3d serve` の Antigravity 上限照会（オプトイン） | `antigravity_quota_probe = true` かつ `GET /api/usage/limits` 呼び出し時、`POST https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary` および `https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary`（空の場合は `fetchAvailableModels`）。`acp_token.json` 利用時のみ `https://oauth2.googleapis.com/token` でアクセストークンを更新します。 | Antigravity のアクセストークン（`Authorization: Bearer`）、`User-Agent`、空の JSON 本文（`acp_token.json` に `project_id` がある場合のみ `project`）。プロンプト・会話内容は含めません。リダイレクトと環境プロキシは無効です。 |
| `t3d status` | `GET /healthz`。既定は `http://127.0.0.1:3774`。明示した `--url` には HTTP(S) で直接接続します。 | ヘルスチェック。認証トークンは付けません。 |
| `t3d mcp` | 標準入力の空でない各行を、`POST /mcp` で既定または `--url` 指定のサーバーに転送します。プロバイダーに注入する MCP 設定の宛先はループバックです。 | JSON-RPC 本文、MCP Bearer トークン、指定された場合は `X-T3-Thread-Id`。CLI クライアントはリダイレクトと環境プロキシを無効にしています。 |
| ブラウザーと macOS の WebView | 選択した t3d のオリジンからページと静的ファイルを取得します。設定・スレッド・実行・検索・添付などの REST API と、ストリーム配信や操作用の `/ws` を同一オリジンで利用します。HTTPS の場合は WebSocket も WSS です。 | 操作に対応するデータ。例としてプロンプト、添付ファイル、許可への応答、認証モードに応じたトークンや Cookie があります。外部リンクを開くと、ブラウザーがリンク先に接続します。 |
| macOS のデスクトップランチャー | 既定のローカルサーバーを使用するときは `127.0.0.1:3774` の `GET /healthz` で稼働と起動完了を確認します。WebView は `T3_APP_URL` で指定したサーバーを読み込めます。 | ローカル起動確認に認証トークンは付けません。WebView の通信はブラウザーと同じです。 |

サーバーは、設定したホストとポートで HTTP、WebSocket、MCP の着信を受け付けます。公開リスナーがループバック以外の場合、プロバイダー用 MCP のリスナーを別途ループバック上に作成します。Tailscale や Cloudflare Tunnel を利用する場合、通信を転送するのは利用者が別途設定したサービスです。`t3d cf-tunnel` は設定例を標準出力に表示します。

## 起動したプロセスの通信

- Claude Code、Codex、Antigravity、OpenCode とは、子プロセスの標準入出力やプロバイダー固有の引数でやり取りします。子プロセスは自分の設定に従い、クラウド API、ローカル推論サーバー、MCP サーバー、実行したツールなどに接続できます。宛先に送信される情報には、要求した処理に応じてプロンプト、会話の引き継ぎ、添付、ワークスペースの内容やツール出力が含まれます。使用量の上限表示（`GET /api/usage/limits`）およびリセットクレジット消費（`POST /api/usage/limits/reset-credit`）でも、`t3d` 自身が `~/.claude.json` や `~/.codex/auth.json` を読むことはなく、Claude CLI の `get_usage` コントロール応答と Codex CLI の `app-server`（`account/rateLimits/read` / `account/rateLimitResetCredit/consume`）を標準入出力経由で照会します（Codex のフォールバック時はローカルのセッションログを参照します）。`t3d` は子プロセスに `DO_NOT_TRACK` などの無効化設定を渡します。個々の CLI やツールの通信はそれぞれの実装と設定に従います。Mock プロバイダーはローカルで動作します。
- Git の差分表示とワークツリー操作はローカルの `git` を起動します。`t3d` からリモートを指定して fetch、push、clone を実行する経路はありません。partial clone で欠けたオブジェクトの取得や、ワークツリーの構成で有効になっているフィルターに伴う通信は Git とその設定に従います。

## ローカル処理

統計、プロジェクトのインポート、プライバシー監査、GC、LaunchAgent 設定の出力はローカルで完結します。モデル一覧の基礎データは同梱され、OpenCode のカタログはローカル設定を読み取ります。プロバイダーの稼働確認でバージョンを取得する際は、各 CLI の `--version` を起動します。ソースの取得や Cargo の依存解決で行われる通信は開発・ビルド時のものです。

実装の参照先は [サーバー認証](../crates/server/src/auth.rs)、[HTTP リスナーと MCP](../crates/server/src/lib.rs)、[CLI クライアント](../crates/cli/src/client.rs)、[Web クライアント](../web/app.js)、[デスクトップ起動確認](../crates/desktop/src/daemon.rs)、[プロバイダー起動](../crates/providers/src/process.rs)、[Git](../crates/git/src/lib.rs) です。
