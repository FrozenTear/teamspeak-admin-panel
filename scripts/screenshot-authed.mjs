#!/usr/bin/env node
// Minimal CDP-driven screenshot harness for authed SPA routes.
//
// Spawns chrome-headless-shell with a debugger, pre-seeds
// `localStorage["ts6-manager.auth.session"]` from a JSON file containing the
// `PersistedSession` shape (PURA-14 `client::store`), then navigates each
// route and writes a PNG.
//
// Usage:
//   node scripts/screenshot-authed.mjs <base-url> <session-json> <out-dir> <route1> [route2 ...]
//
// Example:
//   node scripts/screenshot-authed.mjs http://127.0.0.1:8080 /tmp/session.json screenshots /video-sources

import { spawn } from "node:child_process";
import { mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { existsSync } from "node:fs";

const [, , baseUrl, sessionPath, outDir, ...routes] = process.argv;

if (!baseUrl || !sessionPath || !outDir || routes.length === 0) {
  console.error("usage: screenshot-authed.mjs <base-url> <session-json> <out-dir> <route1> [...]");
  process.exit(64);
}

const SHELL = process.env.HEADLESS_SHELL
  ?? `${process.env.HOME}/.cache/ms-playwright/chromium_headless_shell-1217/chrome-headless-shell-linux64/chrome-headless-shell`;
if (!existsSync(SHELL)) {
  console.error(`chrome-headless-shell not found at ${SHELL}`);
  process.exit(65);
}

const session = JSON.parse(await readFile(sessionPath, "utf8"));
const sessionJson = JSON.stringify(session);

await mkdir(outDir, { recursive: true });
const userDataDir = `${outDir}/.ud-authed`;
await rm(userDataDir, { recursive: true, force: true });

const port = 9222 + Math.floor(Math.random() * 200);
const child = spawn(SHELL, [
  "--no-sandbox",
  "--disable-gpu",
  `--user-data-dir=${userDataDir}`,
  "--window-size=1440,900",
  `--remote-debugging-port=${port}`,
  "--remote-debugging-address=127.0.0.1",
  "about:blank",
], { stdio: ["ignore", "pipe", "pipe"] });

const stop = async () => {
  try { child.kill("SIGTERM"); } catch {}
};
process.on("exit", stop);

// Discover the first page target via the CDP HTTP endpoint.
async function discoverWs() {
  for (let i = 0; i < 40; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (r.ok) {
        const v = await r.json();
        return v.webSocketDebuggerUrl;
      }
    } catch {}
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error("CDP discovery timed out");
}

class CdpClient {
  constructor(ws) {
    this.ws = ws;
    this.next = 1;
    this.pending = new Map();
    this.sessionId = null;
    ws.onmessage = (evt) => {
      const msg = JSON.parse(evt.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(msg.error.message));
        else resolve(msg.result);
      }
    };
  }
  send(method, params = {}) {
    const id = this.next++;
    const frame = { id, method, params };
    if (this.sessionId) frame.sessionId = this.sessionId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify(frame));
    });
  }
}

function openWs(url) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.onopen = () => resolve(ws);
    ws.onerror = (e) => reject(new Error(`WS error: ${e?.message ?? "unknown"}`));
  });
}

async function run() {
  const browserWs = await discoverWs();
  const ws = await openWs(browserWs);
  const browser = new CdpClient(ws);

  // Attach a fresh page target.
  const target = await browser.send("Target.createTarget", { url: "about:blank" });
  const attach = await browser.send("Target.attachToTarget", {
    targetId: target.targetId,
    flatten: true,
  });
  browser.sessionId = attach.sessionId;

  await browser.send("Page.enable");
  await browser.send("Network.enable");

  for (const route of routes) {
    const url = `${baseUrl}${route}`;
    const filename = `${outDir}/PURA-145-ws7-${route.replace(/^\//, "").replace(/\//g, "-") || "index"}.png`;

    // Hit /login first so the origin matches before we touch localStorage.
    await browser.send("Page.navigate", { url: `${baseUrl}/login` });
    await waitLoad(browser);
    await browser.send("Runtime.evaluate", {
      expression: `localStorage.setItem("ts6-manager.auth.session", ${JSON.stringify(sessionJson)})`,
      returnByValue: true,
    });

    // Confirm localStorage stuck on the /login origin.
    const seedCheck = await browser.send("Runtime.evaluate", {
      expression: `localStorage.getItem("ts6-manager.auth.session")`,
      returnByValue: true,
    });
    const seeded = (seedCheck.result?.value ?? "").length;
    console.log(`  /login origin localStorage bytes: ${seeded}`);

    // Now hit the real target.
    await browser.send("Page.navigate", { url });
    await waitLoad(browser);
    // Give the SPA a beat to hydrate + fetch /api/servers and the
    // route's data resource. Release WASM hydrates fast (~300ms) but
    // /api/video-sources may take a beat on the first call.
    await new Promise((r) => setTimeout(r, 9000));

    // Sanity-check storage on the destination origin so we know the
    // hydrate effect saw a valid session blob.
    const check = await browser.send("Runtime.evaluate", {
      expression: `localStorage.getItem("ts6-manager.auth.session")?.slice(0, 32) ?? "<missing>"`,
      returnByValue: true,
    });
    console.log(`  ${route} localStorage head: ${check.result?.value}`);

    // Dump a slice of the rendered DOM + the SPA's current URL so we can
    // confirm whether the auth gate bounced us or the page actually
    // hydrated.
    const dom = await browser.send("Runtime.evaluate", {
      expression: `(()=>{const main=document.querySelector("#main");return JSON.stringify({url:location.pathname+location.search,bodyLen:document.body.innerHTML.length,sampleHTML:document.body.innerHTML.slice(0,400)});})()`,
      returnByValue: true,
    });
    console.log(`  ${route} dom: ${dom.result?.value}`);

    const shot = await browser.send("Page.captureScreenshot", { format: "png" });
    await writeFile(filename, Buffer.from(shot.data, "base64"));
    console.log(`  ${route} → ${filename} (${Buffer.from(shot.data, "base64").byteLength} bytes)`);
  }

  await browser.send("Target.closeTarget", { targetId: target.targetId });
  ws.close();
}

function waitLoad(browser) {
  return new Promise((resolve) => {
    const handler = (evt) => {
      const msg = JSON.parse(evt.data);
      if (msg.method === "Page.loadEventFired") {
        browser.ws.removeEventListener("message", handler);
        resolve();
      }
    };
    browser.ws.addEventListener("message", handler);
    // Fallback timeout — load events occasionally don't fire under
    // chrome-headless-shell, and we still want a screenshot.
    setTimeout(() => {
      browser.ws.removeEventListener("message", handler);
      resolve();
    }, 8000);
  });
}

try {
  await run();
  console.log("OK");
  await stop();
  process.exit(0);
} catch (e) {
  console.error("FAIL:", e?.stack ?? e);
  await stop();
  process.exit(1);
}
