/** Messages from the native companion. It only ever sends text, status and errors. */
export type ListenState = "stopped" | "starting" | "listening" | "stopping" | "error";

export type NativeMessage =
  | { type: "started"; device: string; sampleRate: number; channels: number }
  | { type: "stopped" }
  | {
      type: "status";
      running: boolean;
      state: ListenState;
      deepgramConnected: boolean;
      device: string | null;
    }
  | { type: "transcript_partial"; text: string }
  | { type: "transcript_final"; text: string }
  | { type: "device_changed"; device: string }
  | { type: "error"; code: string; message: string };

/** Commands the extension sends to the native companion. */
export type NativeCommand = { type: "start" } | { type: "stop" } | { type: "status" };

export type CompanionConnection = "connecting" | "connected" | "disconnected";

export interface Snapshot {
  connection: CompanionConnection;
  state: ListenState;
  device: string | null;
  finals: string[];
  partial: string;
  error: { code: string; message: string } | null;
}

/** Popup -> service worker. */
export type PopupCommand =
  | { cmd: "start" }
  | { cmd: "stop" }
  | { cmd: "status" }
  | { cmd: "reconnect" }
  | { cmd: "clear" };

/** Service worker -> popup. */
export type PopupUpdate = { kind: "snapshot"; snapshot: Snapshot };

export const POPUP_PORT_NAME = "popup";
