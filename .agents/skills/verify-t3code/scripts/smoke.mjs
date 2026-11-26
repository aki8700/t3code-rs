import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { get } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { json } from "node:stream/consumers";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath, pathToFileURL } from "node:url";

const repo = fileURLToPath(new URL("../../../../", import.meta.url));
const prompt = "[slow] verify t3 browser reply";
const reply = `mock slow ${prompt}`;
const project = "T3 Browser Smoke";
const timeout = 15_000;
const evidenceRoot = join(repo, "target", "verify-t3code");
await mkdir(evidenceRoot, { recursive: true, mode: 0o700 });
const artifacts = await mkdtemp(join(evidenceRoot, "run-"));
const result = {
  ok: false, stage: "setup", artifacts, thread: null, reply: null,
  pageErrors: [], consoleErrors: [], httpErrors: [], blockedRequests: [], cleanupErrors: [],
};

const abort = new AbortController();
let temporary, server, serverDone, browser, page;
let serverLog = "";
const signals = new Map(["SIGINT", "SIGTERM"].map((signal) => [signal, () => {
  abort.abort(new Error(`Received ${signal}`));
  void browser?.close().catch(() => {});
}]));
for (const [signal, stop] of signals) process.on(signal, stop);

function startDaemon(config, workspace, env, state) {
  const binary = resolve(process.env.T3_VERIFY_BIN || join(repo, "target", "debug", "t3d"));
  server = spawn(binary, [
    "serve", "--config", config, "--data-dir", state, "--host", "127.0.0.1",
    "--port", "0", "--auth", "local-only",
  ], {
    cwd: workspace,
    env: { ...env, RUST_LOG: "t3_server=info", NO_COLOR: "1" },
    stdio: ["ignore", "ignore", "pipe"],
  });
  result.serverPid = server.pid;
  let spawnError;
  server.on("error", (error) => { spawnError = error.message; });
  serverDone = new Promise((done) => {
    server.once("close", (code, signal) => done({ code, signal, spawnError }));
  });
  const listening = new Promise((ready) => {
    server.stderr.setEncoding("utf8").on("data", (chunk) => {
      serverLog = (serverLog + chunk).slice(-64_000);
      const match = /t3d listening[^\n]*address=127\.0\.0\.1:(\d+)/.exec(serverLog);
      if (match) ready(`http://127.0.0.1:${match[1]}`);
    });
  });
  const deadline = AbortSignal.any([abort.signal, AbortSignal.timeout(timeout)]);
  deadline.throwIfAborted();
  return Promise.race([
    listening,
    serverDone.then((exit) => { throw new Error(`t3d exited before listening: ${JSON.stringify(exit)}`); }),
    once(deadline, "abort").then(() => { throw deadline.reason; }),
  ]);
}

async function stopDaemon() {
  if (!server) return;
  // ChildProcess.kill targets only this child. Wait for close before deleting its SQLite files.
  if (server.pid && server.exitCode === null && server.signalCode === null) server.kill("SIGTERM");
  let exit = await Promise.race([serverDone, delay(6000, null, { ref: false })]);
  if (!exit) {
    server.kill("SIGKILL");
    exit = await serverDone;
  }
  result.serverExit = exit;
}

async function assertMockOnly(origin) {
  const signal = AbortSignal.any([abort.signal, AbortSignal.timeout(timeout)]);
  const response = await new Promise((resolve, reject) => {
    get(`${origin}/api/settings`, { agent: false, signal }, resolve).once("error", reject);
  });
  assert.equal(response.statusCode, 200, "Settings preflight must succeed");
  const configuration = await json(response);
  assert.deepEqual(configuration.enabled_providers, ["mock"]);
  assert.equal(configuration.settings.default_model.provider, "mock");
  assert.equal(configuration.settings.default_model.model, "mock");
}

async function capture(name) {
  const settled = await Promise.allSettled([
    page.screenshot({ path: join(artifacts, `${name}.png`), timeout: 4000 }),
    page.locator("body").ariaSnapshot({ timeout: 4000 }).then(async (snapshot) => {
      await writeFile(join(artifacts, `${name}.aria.yml`), snapshot + "\n");
      return snapshot;
    }),
  ]);
  const errors = settled.filter((item) => item.status === "rejected");
  if (errors.length) throw new Error(errors.map(({ reason }) => reason.message).join("\n"));
  return settled[1].value;
}

async function assertTranscript(name) {
  const timeline = page.locator("#timeline");
  await timeline.locator(".msg.assistant:not(.live)").getByText(reply, { exact: true }).waitFor();
  await timeline.locator(".msg.user").getByText(prompt, { exact: true }).waitFor();
  await timeline.locator(".run-foot.completed").waitFor();
  const snapshot = await capture(name);
  assert.ok(snapshot.includes(reply), "The accessibility snapshot must contain the assistant reply");
}

try {
  const modulePath = process.env.PLAYWRIGHT_MJS;
  const { chromium } = await import(modulePath ? pathToFileURL(resolve(modulePath)).href : "playwright");
  temporary = await mkdtemp(join(await realpath(tmpdir()), "t3code-smoke-"));
  const workspace = join(temporary, "workspace");
  const isolatedHome = join(temporary, "home");
  const config = join(temporary, "config.toml");
  const state = join(temporary, "state");
  await mkdir(workspace);
  await mkdir(isolatedHome);
  const env = {
    PATH: process.env.PATH, HOME: isolatedHome, TMPDIR: temporary, GIT_CONFIG_NOSYSTEM: "1",
  };
  const git = { cwd: workspace, env, timeout, stdio: ["ignore", "ignore", "pipe"] };
  // The conversation UI reads the Git diff and the server can checkpoint the workspace.
  execFileSync("git", ["init", "--quiet", "--template="], git);
  execFileSync("git", ["-c", "user.name=T3 Smoke", "-c", "user.email=smoke@example.invalid",
    "commit", "--allow-empty", "--no-gpg-sign", "--quiet", "-m", "Smoke fixture"], git);
  await writeFile(config, `enabled_providers = ["mock"]
fallback_models = []
antigravity_quota_probe = false
antigravity_mcp_enabled = false

[default_model]
provider = "mock"
model = "mock"

[providers]
claude_bin = "disabled/claude"
codex_bin = "disabled/codex"
antigravity_bin = "disabled/agy"
opencode_bin = "disabled/opencode"
`);

  result.stage = "start mock daemon";
  abort.signal.throwIfAborted();
  const origin = await startDaemon(config, workspace, env, state);
  result.origin = origin;
  await assertMockOnly(origin);

  result.stage = "open browser";
  browser = await chromium.launch({ headless: true, timeout, args: ["--no-proxy-server"],
    handleSIGINT: false, handleSIGTERM: false, handleSIGHUP: false });
  abort.signal.throwIfAborted();
  result.chromium = browser.version();
  const context = await browser.newContext({ viewport: { width: 1280, height: 850 }, serviceWorkers: "block" });
  const wsOrigin = origin.replace("http:", "ws:");
  await context.route("**/*", (route) => {
    const url = new URL(route.request().url());
    if (url.origin === origin) return route.continue();
    result.blockedRequests.push(url.href);
    return route.abort("blockedbyclient");
  });
  await context.routeWebSocket((url) => url.origin !== wsOrigin, (socket) => {
    result.blockedRequests.push(socket.url());
    return socket.close();
  });
  page = await context.newPage();
  page.setDefaultTimeout(timeout);
  page.setDefaultNavigationTimeout(timeout);
  page.on("pageerror", (error) => result.pageErrors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") result.consoleErrors.push(message.text());
  });
  page.on("response", (response) => {
    if (response.status() >= 400) {
      result.httpErrors.push(`${response.status()} ${response.request().method()} ${response.url()}`);
    }
  });

  await page.goto(origin);
  await page.locator("#app.ready").waitFor();
  await capture("01-home");
  result.stage = "register project";
  const button = (name) => page.getByRole("button", { name, exact: true });
  await button("+ プロジェクト追加").click();
  const dialog = page.getByRole("dialog", { name: "プロジェクト", exact: true });
  await dialog.getByRole("textbox", { name: "名前", exact: true }).fill(project);
  await dialog.getByRole("textbox", { name: "ワークスペースのパス", exact: true }).fill(workspace);
  await dialog.getByRole("button", { name: "追加", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });

  result.stage = "create thread and send";
  await button("新規スレッド").click();
  await page.getByRole("textbox", { name: "メッセージ", exact: true }).fill(prompt);
  await button("送信").click();
  await page.waitForURL(/#\/t\/[0-9a-f-]{36}$/i);
  result.thread = new URL(page.url()).hash;
  await assertTranscript("02-reply");

  result.stage = "reload thread";
  await page.reload();
  await page.locator("#app.ready").waitFor();
  assert.equal(new URL(page.url()).hash, result.thread);
  await assertTranscript("03-reloaded");
  result.reply = reply;
  result.stage = "check browser diagnostics";
  assert.ok(
    [result.pageErrors, result.consoleErrors, result.httpErrors, result.blockedRequests].every((errors) => !errors.length),
    "The browser reported errors or attempted an external request",
  );
  result.stage = "complete";
} catch (error) {
  result.failure = error.message;
  if (page && !page.isClosed()) {
    await capture("failure").catch((captureError) => { result.captureError = captureError.message; });
  }
} finally {
  const clean = async (operation) => {
    try { await operation(); } catch (error) { result.cleanupErrors.push(error.message); }
  };
  await clean(() => browser?.close());
  await clean(stopDaemon);
  if (temporary) {
    await clean(async () => {
      await rm(temporary, { recursive: true, force: true });
      result.temporaryStateRemoved = true;
    });
  }
  for (const [signal, stop] of signals) process.off(signal, stop);
  if (abort.signal.aborted) result.failure ??= abort.signal.reason.message;
  result.ok = !result.failure && !result.cleanupErrors.length;
  await writeFile(join(artifacts, "t3d.log"), serverLog);
  await writeFile(join(artifacts, "result.json"), JSON.stringify(result, null, 2) + "\n");
  console.log(JSON.stringify(result));
  process.exitCode = result.ok ? 0 : 1;
}
