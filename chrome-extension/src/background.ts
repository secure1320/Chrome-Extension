/**
 * Service worker: owns the Chrome Native Messaging port to the System Audio
 * Companion and keeps the transcript in memory for the popup.
 *
 * Audio never passes through the extension. The companion captures Windows
 * system playback (WASAPI loopback), streams it to Deepgram, and sends only
 * transcript/status/error JSON here.
 */
import {
  POPUP_PORT_NAME,
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
};

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
      break;
    case "transcript_partial":
      snapshot.partial = message.text;
      break;
    case "transcript_final":
      snapshot.finals.push(message.text);
      if (snapshot.finals.length > MAX_FINAL_SEGMENTS) {
        snapshot.finals.splice(0, snapshot.finals.length - MAX_FINAL_SEGMENTS);
      }
      snapshot.partial = "";
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
