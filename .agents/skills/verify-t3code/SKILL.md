---
name: verify-t3code
description: t3code-rs のブラウザ UI を利用者と同じ入口から検証する。プロジェクト登録、新規スレッド、メッセージ送信、応答表示、再読み込み後の履歴に関わる変更や不具合の確認、画面とアクセシブルな状態の採取に使う。
---

# verify-t3code

リポジトリ直下で実行する。Codex では `$verify-t3code`、Claude Code では `/verify-t3code` で呼べる。

```sh
cargo build --locked --bin t3d
node .agents/skills/verify-t3code/scripts/smoke.mjs
```

Git、Node.js 22 以降、Rust ワークスペースをビルドできる環境、Playwright 1.52 以降とその版に対応する Chromium が必要。既存の Playwright は、リポジトリから解決できる `playwright` パッケージか、`PLAYWRIGHT_MJS` で指定した `playwright/index.mjs` を使う。Chromium は Playwright のキャッシュから選ぶ。既存のキャッシュを使う場合は、その環境と同じ `PLAYWRIGHT_BROWSERS_PATH` を引き継ぐ。Cargo の出力先を変更している場合は、`T3_VERIFY_BIN` にこのチェックアウトでビルドした `t3d` を指定する。

ブラウザ用の依存がなければ、リポジトリ外の一時ディレクトリに用意できる。Linux で Chromium のシステムライブラリが不足している場合は、同じ Playwright CLI の `install-deps chromium` で用意する。

```sh
verify_deps=$(mktemp -d)
npm install --prefix "$verify_deps" --no-save --no-package-lock playwright@1.65.1
node "$verify_deps/node_modules/playwright/cli.js" install chromium
PLAYWRIGHT_MJS="$verify_deps/node_modules/playwright/index.mjs" \
  node .agents/skills/verify-t3code/scripts/smoke.mjs
```

## 合格条件

- 空コミットで初期化した一時 Git ワークスペースを画面から登録し、「新規スレッド」のコンポーザーから送信する。初回送信でスレッドが作られる。
- タイムラインの確定したアシスタント欄に Mock の期待応答が出る。同じスレッドをブラウザで再読み込みして、送信内容と応答が再表示される。
- ページ例外、`console.error`、HTTP 4xx/5xx、遮断された外部リクエストがすべて 0 件で終了コードが 0。

スモーク専用の `t3d` はループバックの空きポートで起動し、一時ホームと一時 SQLite 状態を使う。設定の `enabled_providers` を Mock のみにする。`--enable-mock` は既定のプロバイダーに Mock を追加するため、専用設定を使う。ページは専用のヘッドレス Chromium で開き、その `t3d` への通信だけを許可する。終了時はこの実行のブラウザとデーモンを閉じ、一時状態とワークスペースを削除する。

## 証拠と確認範囲

標準出力の JSON にある `artifacts` が保存先。 `target/verify-t3code/run-*` に送信前、応答後、再読み込み後の PNG と ARIA スナップショット、`result.json`、`t3d.log` が残る。失敗時は `stage` と各エラーを確認し、取得できれば `failure.png` と `failure.aria.yml` も読む。

変更固有の操作を確認するときは、対象の入口を実際に開き、操作前後の画面と ARIA 状態、関係する保存結果を記録する。読み取りで補う場合は同じ専用環境の状態を使う。報告には入口、実行コマンド、期待した表示、再読み込み後の結果、エラー件数、証拠のパスを書く。標準のスモークがカバーする範囲はブラウザでの会話と履歴の往復。
