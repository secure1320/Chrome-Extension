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
const MAX_BLOCKS = 500;
/** Silence (no partial or final) that starts a new timestamped block. */
const PAUSE_MS = 3000;

let nativePort: chrome.runtime.Port | null = null;
let retryAttempt = 0;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
const popupPorts = new Set<chrome.runtime.Port>();

const snapshot: Snapshot = {
  connection: "disconnected",
  state: "stopped",
  device: null,
  blocks: [],
  partial: "",
  partialBlockAt: null,
  error: null,
  target: null,
  capture: null,
};

let sessionStart: number | null = null;
let lastActivity = 0;
/** The sentence currently being spoken: when it started and whether it opens a new block. */
let segment: { at: number; newBlock: boolean } | null = null;

function beginSegment(now: number): { at: number; newBlock: boolean } {
  sessionStart ??= now;
  segment ??= {
    at: now - sessionStart,
    newBlock: snapshot.blocks.length === 0 || now - lastActivity >= PAUSE_MS,
  };
  lastActivity = now;
  return segment;
}

function endSegment(): void {
  segment = null;
  snapshot.partial = "";
  snapshot.partialBlockAt = null;
}

function appendFinal(text: string): void {
  const { at, newBlock } = beginSegment(Date.now());
  const last = snapshot.blocks[snapshot.blocks.length - 1];
  if (newBlock || !last) {
    snapshot.blocks.push({ at, text });
    if (snapshot.blocks.length > MAX_BLOCKS) snapshot.blocks.splice(0, snapshot.blocks.length - MAX_BLOCKS);
  } else {
    last.text += ` ${text}`;
  }
  endSegment();
}

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
/** Bumped on unlock/clear so queued inserts that started earlier become no-ops. */
let insertEpoch = 0;
/** Committed transcript text successfully reflected in the locked field (no live partial). */
let typedCommitted = "";

/** Side-panel committed transcript: blocks joined with a space. */
function committedText(): string {
  return snapshot.blocks.map((b) => b.text).filter(Boolean).join(" ");
}

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
      endSegment();
      forwardTranscript("", false);
      break;
    case "transcript_partial": {
      const { at, newBlock } = beginSegment(Date.now());
      snapshot.partial = message.text;
      snapshot.partialBlockAt = newBlock ? at : null;
      forwardTranscript(message.text, false);
      break;
    }
    case "transcript_final":
      appendFinal(message.text);
      forwardTranscript(message.text, true);
      break;
    case "device_changed":
      snapshot.device = message.device;
      break;
    case "screen_captured":
      snapshot.capture = { busy: false, failed: false, message: captureSummary(message) };
      break;
    case "error":
      if (message.code === "SCREEN_CAPTURE_FAILED") {
        snapshot.capture = { busy: false, failed: true, message: message.message };
      } else if (message.code === "INVALID_COMMAND" && snapshot.capture?.busy) {
        snapshot.capture = {
          busy: false,
          failed: true,
          message: "The companion is out of date. Run native-companion\\installer\\install.ps1 -Build again.",
        };
      } else {
        snapshot.error = { code: message.code, message: message.message };
      }
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
  endSegment();
  if (snapshot.capture?.busy) {
    snapshot.capture = { busy: false, failed: true, message: "The companion disconnected during the capture." };
  }
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
  sessionStart ??= Date.now();
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

function captureSummary(result: Extract<NativeMessage, { type: "screen_captured" }>): string {
  const name = result.path?.split("\\").pop();
  if (name && result.copied) return `Saved ${name} to Downloads and copied it to the clipboard.`;
  if (name) return `Saved ${name} to Downloads. Copying to the clipboard failed.`;
  return "Copied to the clipboard. Saving to Downloads failed.";
}

export function captureScreen(): void {
  if (snapshot.capture?.busy) return;
  if (!nativePort) connectCompanion();
  snapshot.capture = postNative({ type: "capture_screen" })
    ? { busy: true, failed: false, message: "Capturing the screen…" }
    : { busy: false, failed: true, message: "The companion is not connected." };
  broadcast();
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

function enqueue(task: (epoch: number) => Promise<void>): void {
  const epoch = insertEpoch;
  insertQueue = insertQueue
    .then(async () => {
      if (epoch !== insertEpoch) return;
      await task(epoch);
    })
    .catch((error) => console.error("Text insertion failed:", error));
}

function stillCurrent(epoch: number): boolean {
  return epoch === insertEpoch;
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

/** Deliver a message, rediscovering the frame if needed. Returns false if the epoch was cancelled. */
async function deliver(
  target: InsertTarget,
  message: InserterMessage,
  epoch: number,
): Promise<boolean | "cancelled"> {
  if (!stillCurrent(epoch)) return "cancelled";
  let delivered = await sendToFrame(target, message);
  if (!stillCurrent(epoch)) return "cancelled";
  if (!delivered) {
    target.frameId = await findEditableFrame(target.tabId);
    if (!stillCurrent(epoch)) return "cancelled";
    delivered = await sendToFrame(target, message);
  }
  return delivered;
}

function lockTarget(): void {
  enqueue(async (epoch) => {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (!stillCurrent(epoch) || tab?.id === undefined) return;
    const target: InsertTarget = {
      tabId: tab.id,
      frameId: null,
      title: tab.title || tab.url || "Untitled tab",
      problem: null,
    };
    insertTarget = target;
    try {
      target.frameId = await findEditableFrame(tab.id);
      if (!stillCurrent(epoch) || insertTarget !== target) return;

      let desired = committedText();
      if (!desired.startsWith(typedCommitted)) {
        // Transcript was cleared/trimmed out from under us: full resync of committed text.
        typedCommitted = "";
        const reset = await deliver(target, { type: "sac-reset" }, epoch);
        if (reset === "cancelled") return;
        if (!reset) {
          target.problem = NO_TEXT_BOX;
          publishTarget();
          return;
        }
        desired = committedText();
      }

      const missing = desired.slice(typedCommitted.length);
      if (missing) {
        const committed = await deliver(target, { type: "sac-commit", text: missing }, epoch);
        if (committed === "cancelled") return;
        if (!committed) {
          target.problem = NO_TEXT_BOX;
          publishTarget();
          return;
        }
      } else {
        // Nothing to catch up: still verify the box is reachable.
        const probe = await deliver(target, { type: "sac-sync", text: "", final: false }, epoch);
        if (probe === "cancelled") return;
        if (!probe) {
          target.problem = NO_TEXT_BOX;
          publishTarget();
          return;
        }
      }

      typedCommitted = desired;
      target.problem = null;
      publishTarget();

      if (snapshot.partial && stillCurrent(epoch) && insertTarget === target) {
        const live = await deliver(target, { type: "sac-sync", text: snapshot.partial, final: false }, epoch);
        if (live === "cancelled") return;
        setProblem(target, live ? null : NO_TEXT_BOX);
      }
    } catch {
      if (stillCurrent(epoch) && insertTarget === target) {
        target.problem = "Chrome doesn't allow extensions to type on this page.";
        publishTarget();
      }
    }
  });
}

function unlockTarget(): void {
  const previous = insertTarget;
  insertTarget = null;
  insertEpoch++;
  publishTarget();
  // Strip the grey live segment immediately; leave committed text and typedCommitted alone.
  if (previous?.frameId !== null && previous) {
    void sendToFrame(previous, { type: "sac-sync", text: "", final: false });
  }
}

function forwardTranscript(text: string, final: boolean): void {
  if (!insertTarget) return;
  enqueue(async (epoch) => {
    const target = insertTarget;
    if (!stillCurrent(epoch) || !target) return;
    if (!text && !final && target.frameId === null) return;
    const message: InserterMessage = { type: "sac-sync", text, final };
    try {
      const delivered = await deliver(target, message, epoch);
      if (delivered === "cancelled") return;
      if (delivered && final) typedCommitted = committedText();
      if (stillCurrent(epoch) && insertTarget === target) {
        setProblem(target, delivered ? null : NO_TEXT_BOX);
      }
    } catch {
      if (stillCurrent(epoch) && insertTarget === target) {
        setProblem(target, "Can't reach the locked tab. Click its text box and lock again.");
      }
    }
  });
}

function clearTranscriptAndTyped(): void {
  const target = insertTarget;
  const retractCount = typedCommitted.length;
  snapshot.blocks = [];
  endSegment();
  typedCommitted = "";
  insertEpoch++;
  sessionStart = snapshot.state === "listening" || snapshot.state === "starting" ? Date.now() : null;
  if (target) {
    void sendToFrame(target, { type: "sac-retract", count: retractCount });
  }
  broadcast();
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
        clearTranscriptAndTyped();
        break;
      case "lock-target":
        lockTarget();
        break;
      case "unlock-target":
        unlockTarget();
        break;
      case "capture-screen":
        captureScreen();
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
