// End-to-end check through a real Chrome: loads the unpacked extension into a
// throwaway headless profile (DevTools pipe), installs the native host for that
// extension ID, then clicks Start/Stop in the real popup page.
//
//   node tools/chrome-e2e.mjs [--chrome path\to\chrome.exe] [--speak] [--screenshot out.png]
//
// The companion inherits this process's environment, so DEEPGRAM_API_KEY (or
// SYSTEM_AUDIO_DEEPGRAM_ENDPOINT for a mock server) is picked up as usual.

import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..", "..");
const extensionDir = path.join(repo, "chrome-extension");
const installer = path.join(repo, "native-companion", "installer", "install.ps1");
const args = process.argv.slice(2);
const chromeArg = args.indexOf("--chrome");
const chromePath =
  chromeArg >= 0 ? args[chromeArg + 1] : "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
const speak = args.includes("--speak");
const shotArg = args.indexOf("--screenshot");
const screenshotPath = shotArg >= 0 ? path.resolve(args[shotArg + 1]) : null;

const profile = mkdtempSync(path.join(tmpdir(), "sac-chrome-"));
const chrome = spawn(
  chromePath,
  [
    `--user-data-dir=${profile}`,
    "--headless=new",
    "--remote-debugging-pipe",
    "--enable-unsafe-extension-debugging",
    "--no-first-run",
    "--no-default-browser-check",
    "about:blank",
  ],
  { stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] },
);

const toChrome = chrome.stdio[3];
const fromChrome = chrome.stdio[4];
let nextId = 1;
const pending = new Map();
let inbuf = "";
fromChrome.on("data", (d) => {
  inbuf += d.toString();
  let idx;
  while ((idx = inbuf.indexOf("\0")) >= 0) {
    const msg = JSON.parse(inbuf.slice(0, idx));
    inbuf = inbuf.slice(idx + 1);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(`${msg.error.message}`)) : resolve(msg.result);
    }
  }
});

function cdp(method, params = {}, sessionId) {
  const id = nextId++;
  toChrome.write(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }) + "\0");
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (cond, label) => {
  console.log(`${cond ? "PASS" : "FAIL"}: ${label}`);
  if (!cond) failures.push(label);
};

async function evaluate(sessionId, expression) {
  const r = await cdp("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }, sessionId);
  return r.result.value;
}

async function waitForText(sessionId, selector, pred, ms, label) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    last = await evaluate(sessionId, `document.querySelector(${JSON.stringify(selector)})?.textContent ?? ""`);
    if (pred(last)) return last;
    await sleep(200);
  }
  throw new Error(`timeout waiting for ${label} (last: ${JSON.stringify(last)})`);
}

try {
  const { id: extensionId } = await cdp("Extensions.loadUnpacked", { path: extensionDir });
  console.log(`Loaded extension ${extensionId}`);

  const install = spawnSync(
    "powershell.exe",
    ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", installer, "-ExtensionId", extensionId],
    { encoding: "utf8" },
  );
  process.stdout.write(install.stdout);
  check(install.status === 0, "install.ps1 succeeded");
  if (install.status !== 0) throw new Error(install.stderr);

  const { targetId } = await cdp("Target.createTarget", { url: `chrome-extension://${extensionId}/popup.html` });
  const { sessionId } = await cdp("Target.attachToTarget", { targetId, flatten: true });

  const connected = await waitForText(sessionId, "#status-text", (t) => t === "Companion connected", 10000, "Companion connected");
  check(connected === "Companion connected", "popup shows Companion connected");
  const device = await evaluate(sessionId, `document.querySelector("#device").textContent`);
  check(device.length > 0, `popup shows output device (${device})`);

  await evaluate(sessionId, `document.querySelector("#toggle").click()`);
  const outcome = await waitForText(
    sessionId,
    "#status-text, #error",
    (t) => t === "Listening",
    15000,
    "Listening",
  ).catch(async (e) => {
    const err = await evaluate(sessionId, `document.querySelector("#error").textContent`);
    throw new Error(`${e.message}; error box: ${err}`);
  });
  check(outcome === "Listening", "popup shows Listening after Start");
  check(
    (await evaluate(sessionId, `document.querySelector("#toggle").textContent`)) === "Stop Listening",
    "button switches to Stop Listening",
  );

  if (speak) {
    spawnSync("powershell.exe", [
      "-NoProfile",
      "-Command",
      'Add-Type -AssemblyName System.Speech; (New-Object System.Speech.Synthesis.SpeechSynthesizer).Speak("Hello everyone, thanks for joining the meeting today.")',
    ]);
    const text = await waitForText(sessionId, "#transcript .text", (t) => t.length > 0, 8000, "final transcript");
    check(text.length > 0, `transcript displayed: ${JSON.stringify(text)}`);
    if (screenshotPath) {
      await cdp("Emulation.setDeviceMetricsOverride", { width: 392, height: 420, deviceScaleFactor: 1, mobile: false }, sessionId);
      const { data } = await cdp("Page.captureScreenshot", { format: "png" }, sessionId);
      writeFileSync(screenshotPath, Buffer.from(data, "base64"));
      console.log(`Screenshot saved to ${screenshotPath}`);
    }
  } else {
    await sleep(2000);
  }

  await evaluate(sessionId, `document.querySelector("#toggle").click()`);
  const stopped = await waitForText(sessionId, "#status-text", (t) => t === "Companion connected", 10000, "stopped");
  check(stopped === "Companion connected", "popup returns to Companion connected after Stop");
  check(
    (await evaluate(sessionId, `document.querySelector("#toggle").textContent`)) === "Start Listening",
    "button switches back to Start Listening",
  );
} catch (e) {
  failures.push(String(e));
  console.log("FAIL:", e.message);
} finally {
  await cdp("Browser.close").catch(() => {});
  await sleep(1500);
  chrome.kill();
  try {
    rmSync(profile, { recursive: true, force: true });
  } catch {}
}

console.log(failures.length ? `\n${failures.length} check(s) failed` : "\nAll Chrome checks passed");
process.exit(failures.length ? 1 : 0);
