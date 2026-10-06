import { POPUP_PORT_NAME, type PopupCommand, type PopupUpdate, type Snapshot } from "./messages.js";

const HOST_NOT_FOUND = "Specified native messaging host not found.";

function el<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing #${id}`);
  return node as T;
}

const dot = el<HTMLSpanElement>("status-dot");
const statusText = el<HTMLSpanElement>("status-text");
const device = el<HTMLDivElement>("device");
const errorBox = el<HTMLDivElement>("error");
const transcriptSection = el<HTMLElement>("transcript-section");
const transcript = el<HTMLDivElement>("transcript");
const toggle = el<HTMLButtonElement>("toggle");
const clear = el<HTMLButtonElement>("clear");
const reconnect = el<HTMLButtonElement>("reconnect");
const lock = el<HTMLButtonElement>("lock");
const capture = el<HTMLButtonElement>("capture");

const port = chrome.runtime.connect({ name: POPUP_PORT_NAME });
const send = (command: PopupCommand) => port.postMessage(command);

let current: Snapshot | null = null;
/** Lock state requested by a click, until the service worker confirms it. */
let lockRequested: boolean | null = null;
let lockRequestTimer: ReturnType<typeof setTimeout> | undefined;
let captureWasBusy = false;
let captureResultTimer: ReturnType<typeof setTimeout> | undefined;

/** Replays the one-shot click animation. */
function flash(button: HTMLButtonElement): void {
  button.classList.remove("flash");
  void button.offsetWidth;
  button.classList.add("flash");
}

function showCaptureResult(failed: boolean): void {
  clearTimeout(captureResultTimer);
  capture.classList.toggle("done", !failed);
  capture.classList.toggle("failed", failed);
  capture.textContent = failed ? "Failed" : "Saved";
  captureResultTimer = setTimeout(() => {
    capture.classList.remove("done", "failed");
    capture.textContent = "Capture";
  }, failed ? 3000 : 1800);
}

function statusLabel(s: Snapshot): { text: string; tone: string } {
  if (s.connection === "disconnected") return { text: "Companion disconnected", tone: "off" };
  if (s.connection === "connecting") return { text: "Connecting to companion…", tone: "pending" };
  switch (s.state) {
    case "listening":
      return { text: "Listening", tone: "live" };
    case "starting":
      return { text: "Starting…", tone: "pending" };
    case "stopping":
      return { text: "Stopping…", tone: "pending" };
    case "error":
    case "stopped":
      return { text: "Companion connected", tone: "ok" };
  }
}

function formatTime(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

function setText(node: Element, text: string): void {
  if (node.textContent !== text) node.textContent = text;
}

/** Updates the block elements in place so a new partial doesn't rebuild the whole transcript. */
function renderTranscript(s: Snapshot): void {
  const rows = s.blocks.map((b) => ({ at: b.at, text: b.text, partial: "" }));
  if (s.partial) {
    const last = rows[rows.length - 1];
    if (s.partialBlockAt !== null || !last) rows.push({ at: s.partialBlockAt ?? 0, text: "", partial: s.partial });
    else last.partial = s.partial;
  }

  while (transcript.children.length > rows.length) transcript.lastElementChild?.remove();
  rows.forEach((row, i) => {
    let block = transcript.children[i];
    if (!block) {
      block = document.createElement("div");
      block.className = "block";
      for (const cls of ["ts", "text", "partial"]) {
        const span = document.createElement("span");
        span.className = cls;
        block.append(span);
      }
      transcript.append(block);
    }
    const [ts, text, partial] = block.children;
    setText(ts, `[${formatTime(row.at)}]`);
    setText(text, row.text);
    setText(partial, row.partial ? (row.text ? " " : "") + row.partial : "");
  });
}

function render(s: Snapshot): void {
  current = s;
  const { text, tone } = statusLabel(s);
  statusText.textContent = text;
  dot.dataset.tone = tone;

  device.textContent = s.device ?? "";
  device.hidden = !s.device;

  if (s.error) {
    errorBox.hidden = false;
    errorBox.textContent =
      s.error.message === HOST_NOT_FOUND
        ? "System Audio Companion is not installed. Run native-companion\\installer\\install.ps1."
        : s.error.message;
  } else {
    errorBox.hidden = true;
    errorBox.textContent = "";
  }

  const hasText = s.blocks.length > 0 || s.partial.length > 0;
  transcriptSection.hidden = !hasText && s.state !== "listening";
  const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 24;
  renderTranscript(s);
  if (atBottom) transcript.scrollTop = transcript.scrollHeight;

  const active = s.state === "listening" || s.state === "starting";
  const transitioning = s.state === "starting" || s.state === "stopping" || s.connection === "connecting";
  toggle.textContent = active ? "Stop Listening" : "Start Listening";
  toggle.classList.toggle("stop", active);
  toggle.classList.toggle("busy", transitioning);
  toggle.classList.toggle("glow", s.state === "listening");
  toggle.disabled = s.connection === "connecting" || s.state === "stopping";
  clear.disabled = !hasText;
  reconnect.hidden = s.connection !== "disconnected";

  const locked = s.target !== null;
  if (lockRequested !== null && lockRequested === locked) {
    lockRequested = null;
    clearTimeout(lockRequestTimer);
  }
  const problem = s.target?.problem ?? null;
  lock.textContent = locked ? "Locked" : "Lock";
  lock.classList.toggle("busy", lockRequested !== null);
  lock.classList.toggle("on", locked && !problem);
  lock.classList.toggle("warn", problem !== null);
  lock.classList.toggle("glow", locked && s.state === "listening");
  lock.title = problem
    ? problem
    : locked
      ? `Typing into: ${s.target?.title}. Click to unlock.`
      : "Click a text box on the page, then lock this tab.";

  const captureBusy = s.capture?.busy === true;
  capture.classList.toggle("busy", captureBusy);
  capture.disabled = captureBusy || s.connection === "connecting";
  if (captureWasBusy && !captureBusy && s.capture) showCaptureResult(s.capture.failed);
  captureWasBusy = captureBusy;
  capture.title = s.capture && !captureBusy ? s.capture.message : "Capture screen";
}

port.onMessage.addListener((update: PopupUpdate) => {
  if (update.kind === "snapshot") render(update.snapshot);
});

for (const button of [toggle, clear, reconnect, lock, capture]) {
  button.addEventListener("animationend", (e) => {
    if (e.animationName === "flash") button.classList.remove("flash");
  });
}

toggle.addEventListener("click", () => {
  const active = current?.state === "listening" || current?.state === "starting";
  flash(toggle);
  send({ cmd: active ? "stop" : "start" });
});
clear.addEventListener("click", () => {
  flash(clear);
  send({ cmd: "clear" });
});
reconnect.addEventListener("click", () => {
  flash(reconnect);
  send({ cmd: "reconnect" });
});
lock.addEventListener("click", () => {
  const lockNow = !current?.target;
  lockRequested = lockNow;
  lock.classList.add("busy");
  clearTimeout(lockRequestTimer);
  lockRequestTimer = setTimeout(() => {
    lockRequested = null;
    lock.classList.remove("busy");
  }, 3000);
  send({ cmd: lockNow ? "lock-target" : "unlock-target" });
});
capture.addEventListener("click", () => {
  clearTimeout(captureResultTimer);
  capture.classList.remove("done", "failed");
  capture.textContent = "Capture";
  send({ cmd: "capture-screen" });
});
