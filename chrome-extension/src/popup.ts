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
const finals = el<HTMLSpanElement>("finals");
const partial = el<HTMLSpanElement>("partial");
const toggle = el<HTMLButtonElement>("toggle");
const clear = el<HTMLButtonElement>("clear");
const reconnect = el<HTMLButtonElement>("reconnect");
const targetText = el<HTMLSpanElement>("target-text");
const targetProblem = el<HTMLDivElement>("target-problem");
const lock = el<HTMLButtonElement>("lock");

const port = chrome.runtime.connect({ name: POPUP_PORT_NAME });
const send = (command: PopupCommand) => port.postMessage(command);

let current: Snapshot | null = null;

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

  const hasText = s.finals.length > 0 || s.partial.length > 0;
  transcriptSection.hidden = !hasText && s.state !== "listening";
  const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 24;
  finals.textContent = s.finals.join(" ");
  partial.textContent = s.partial ? (s.finals.length ? " " : "") + s.partial : "";
  if (atBottom) transcript.scrollTop = transcript.scrollHeight;

  const active = s.state === "listening" || s.state === "starting";
  toggle.textContent = active ? "Stop Listening" : "Start Listening";
  toggle.classList.toggle("stop", active);
  toggle.disabled = s.connection === "connecting" || s.state === "stopping";
  clear.disabled = !hasText;
  reconnect.hidden = s.connection !== "disconnected";

  if (s.target) {
    targetText.textContent = `Typing into: ${s.target.title}`;
    targetText.title = s.target.title;
    lock.textContent = "Unlock";
  } else {
    targetText.textContent = "Click a text box on the page, then lock it here.";
    targetText.title = "";
    lock.textContent = "Lock to this tab";
  }
  targetProblem.hidden = !s.target?.problem;
  targetProblem.textContent = s.target?.problem ?? "";
}

port.onMessage.addListener((update: PopupUpdate) => {
  if (update.kind === "snapshot") render(update.snapshot);
});

toggle.addEventListener("click", () => {
  const active = current?.state === "listening" || current?.state === "starting";
  send({ cmd: active ? "stop" : "start" });
});
clear.addEventListener("click", () => send({ cmd: "clear" }));
reconnect.addEventListener("click", () => send({ cmd: "reconnect" }));
lock.addEventListener("click", () => send({ cmd: current?.target ? "unlock-target" : "lock-target" }));
