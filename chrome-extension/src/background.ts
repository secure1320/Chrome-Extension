/**
 * Service worker: owns the Chrome Native Messaging port to the System Audio
 * Companion, keeps the transcript in memory for the side panel, and forwards it
 * to the content script of the tab the user locked for typing.
 *
 * Audio never passes through the extension. The companion captures Windows
 * system playback (WASAPI loopback), streams it to Deepgram, and sends only
 * transcript/status/error JSON here.
 */
import {
  POPUP_PORT_NAME,
  type InserterMessage,
  type InserterProbe,
  type NativeCommand,
  type NativeMessage,
  type PopupCommand,
  type PopupUpdate,
  type Snapshot,
} from "./messages.js";

const NATIVE_HOST = "com.systemaudio.deepgram";
const RETRY_DELAYS_MS = [1000, 2000, 4000];
const MAX_FINAL_SEGMENTS = 500;

let nativePort: chrome.runtime.Port | null = null;
let retryAttempt = 0;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
const popupPorts = new Set<chrome.runtime.Port>();

const snapshot: Snapshot = {
  connection: "disconnected",
  state: "stopped",
  device: null,
  finals: [],
  partial: "",
  error: null,
  target: null,
};

interface InsertTarget {
  tabId: number;
  frameId: number | null;
  title: string;
  problem: string | null;
}

const NO_TEXT_BOX = "Click a text box in the locked tab. Text will go there.";
let insertTarget: InsertTarget | null = null;
/** Keeps page updates in arrival order even though delivering each one is async. */
let insertQueue: Promise<void> = Promise.resolve();

function broadcast(): void {
  const update: PopupUpdate = { kind: "snapshot", snapshot };
  for (const port of popupPorts) {
    try {
      port.postMessage(update);
    } catch {
      popupPorts.delete(port);
    }
  }
}

function postNative(command: NativeCommand): boolean {
  if (!nativePort) return false;
  try {
    nativePort.postMessage(command);
    return true;
  } catch {
    return false;
  }
}

function clearRetry(): void {
  if (retryTimer !== null) {
    clearTimeout(retryTimer);
    retryTimer = null;
  }
}

export function connectCompanion(): void {
  if (nativePort) return;
  clearRetry();
  snapshot.connection = "connecting";
  broadcast();

  const port = chrome.runtime.connectNative(NATIVE_HOST);
  nativePort = port;
  port.onMessage.addListener(onNativeMessage);
  port.onDisconnect.addListener(onNativeDisconnect);
  getStatus();
}

function onNativeMessage(message: NativeMessage): void {
  retryAttempt = 0;
  snapshot.connection = "connected";
  if (snapshot.error?.code === "COMPANION_DISCONNECTED") snapshot.error = null;

  switch (message.type) {
    case "status":
      snapshot.state = message.state;
      snapshot.device = message.device;
      break;
    case "started":
      snapshot.state = "listening";
      snapshot.device = message.device;
      snapshot.error = null;
      break;
    case "stopped":
      snapshot.state = "stopped";
      snapshot.partial = "";
      forwardTranscript("", false);
      break;
    case "transcript_partial":
      snapshot.partial = message.text;
      forwardTranscript(message.text, false);
      break;
    case "transcript_final":
      snapshot.finals.push(message.text);
      if (snapshot.finals.length > MAX_FINAL_SEGMENTS) {
        snapshot.finals.splice(0, snapshot.finals.length - MAX_FINAL_SEGMENTS);
      }
      snapshot.partial = "";
      forwardTranscript(message.text, true);
      break;
    case "device_changed":
      snapshot.device = message.device;
      break;
    case "error":
      snapshot.error = { code: message.code, message: message.message };
      break;
  }
  broadcast();
}

function onNativeDisconnect(): void {
  const reason = chrome.runtime.lastError?.message ?? "Companion exited.";
  console.warn("System Audio Companion disconnected:", reason);
  nativePort = null;
  snapshot.connection = "disconnected";
  snapshot.state = "stopped";
  snapshot.partial = "";
  snapshot.error = { code: "COMPANION_DISCONNECTED", message: reason };
  broadcast();
  forwardTranscript("", false);
  scheduleRetry();
}

/** Small bounded backoff; after that the user reconnects manually from the popup. */
function scheduleRetry(): void {
  if (retryTimer !== null || retryAttempt >= RETRY_DELAYS_MS.length) return;
  const delay = RETRY_DELAYS_MS[retryAttempt++];
  retryTimer = setTimeout(() => {
    retryTimer = null;
    connectCompanion();
  }, delay);
}

export function startListening(): void {
  if (!nativePort) connectCompanion();
  snapshot.error = null;
  if (snapshot.state === "stopped" || snapshot.state === "error") {
    snapshot.state = "starting";
  }
  broadcast();
  postNative({ type: "start" });
}

export function stopListening(): void {
  if (snapshot.state === "listening" || snapshot.state === "starting") {
    snapshot.state = "stopping";
    broadcast();
  }
  postNative({ type: "stop" });
}

export function getStatus(): void {
  postNative({ type: "status" });
}

function publishTarget(): void {
  snapshot.target = insertTarget ? { title: insertTarget.title, problem: insertTarget.problem } : null;
  broadcast();
}

function setProblem(target: InsertTarget, problem: string | null): void {
  if (target !== insertTarget || target.problem === problem) return;
  target.problem = problem;
  publishTarget();
}

function enqueue(task: () => Promise<void>): void {
  insertQueue = insertQueue.then(task).catch((error) => console.error("Text insertion failed:", error));
}

/** Runs inside each frame of the page, in the content script's world. */
function probeInserter(): InserterProbe | null {
  return (globalThis as { __sacInserter?: { probe(): InserterProbe } }).__sacInserter?.probe() ?? null;
}

/** The frame holding the most recently focused text box, injecting the content script if the tab predates it. */
async function findEditableFrame(tabId: number): Promise<number | null> {
  const probe = () => chrome.scripting.executeScript({ target: { tabId, allFrames: true }, func: probeInserter });
  let results = await probe();
  if (results.every((r) => r.result == null)) {
    await chrome.scripting.executeScript({ target: { tabId, allFrames: true }, files: ["dist/content.js"] });
    results = await probe();
  }
  let best: { frameId: number; focusedAt: number } | null = null;
  for (const { frameId, result } of results) {
    if (!result?.editable) continue;
    if (!best || result.focusedAt > best.focusedAt) best = { frameId, focusedAt: result.focusedAt };
  }
  return best?.frameId ?? null;
}

async function sendToFrame(target: InsertTarget, message: InserterMessage): Promise<boolean> {
  if (target.frameId === null) return false;
  try {
    return (await chrome.tabs.sendMessage(target.tabId, message, { frameId: target.frameId })) === true;
  } catch {
    return false;
  }
}

function lockTarget(): void {
  enqueue(async () => {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (tab?.id === undefined) return;
    const target: InsertTarget = {
      tabId: tab.id,
      frameId: null,
      title: tab.title || tab.url || "Untitled tab",
      problem: null,
    };
    insertTarget = target;
    try {
      target.frameId = await findEditableFrame(tab.id);
      if (!(await sendToFrame(target, { type: "sac-reset" }))) target.problem = NO_TEXT_BOX;
    } catch {
      target.problem = "Chrome doesn't allow extensions to type on this page.";
    }
    publishTarget();
  });
}

function unlockTarget(): void {
  insertTarget = null;
  publishTarget();
}

function forwardTranscript(text: string, final: boolean): void {
  if (!insertTarget) return;
  enqueue(async () => {
    const target = insertTarget;
    if (!target || (!text && !final && target.frameId === null)) return;
    const message: InserterMessage = { type: "sac-sync", text, final };
    try {
      let delivered = await sendToFrame(target, message);
      if (!delivered) {
        target.frameId = await findEditableFrame(target.tabId);
        delivered = await sendToFrame(target, message);
      }
      setProblem(target, delivered ? null : NO_TEXT_BOX);
    } catch {
      setProblem(target, "Can't reach the locked tab. Click its text box and lock again.");
    }
  });
}

chrome.tabs.onRemoved.addListener((tabId) => {
  if (insertTarget?.tabId === tabId) unlockTarget();
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (insertTarget?.tabId === tabId && changeInfo.title) {
    insertTarget.title = changeInfo.title;
    publishTarget();
  }
});

chrome.sidePanel
  .setPanelBehavior({ openPanelOnActionClick: true })
  .catch((error) => console.error("Failed to set side panel behavior:", error));

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== POPUP_PORT_NAME) return;
  popupPorts.add(port);
  port.onDisconnect.addListener(() => popupPorts.delete(port));
  port.onMessage.addListener((command: PopupCommand) => {
    switch (command.cmd) {
      case "start":
        startListening();
        break;
      case "stop":
        stopListening();
        break;
      case "status":
        getStatus();
        break;
      case "reconnect":
        retryAttempt = 0;
        connectCompanion();
        break;
      case "clear":
        snapshot.finals = [];
        snapshot.partial = "";
        broadcast();
        break;
      case "lock-target":
        lockTarget();
        break;
      case "unlock-target":
        unlockTarget();
        break;
    }
  });

  if (!nativePort) {
    retryAttempt = 0;
    connectCompanion();
  } else {
    getStatus();
  }
  port.postMessage({ kind: "snapshot", snapshot } satisfies PopupUpdate);
});
