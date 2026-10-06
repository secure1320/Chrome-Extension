// Drives the companion exactly like Chrome does (framed JSON over stdio) and checks
// the protocol: idempotent start/stop/status, transcripts, clean exit on EOF.
//
//   node tools/nm-test.mjs [path\to\system-audio-companion.exe] [--speak]
//
// --speak plays Windows text-to-speech through the speakers while listening, so the
// WASAPI loopback -> Deepgram path produces transcripts.

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const exe =
  args.find((a) => !a.startsWith("--")) ??
  path.join(here, "..", "target", "release", "system-audio-companion.exe");
const speak = args.includes("--speak");

const child = spawn(exe, ["chrome-extension://nmtestextensionidaaaaaaaaaaaaaaaa/", "--parent-window=0"], {
  stdio: ["pipe", "pipe", "pipe"],
});

const received = [];
const waiters = [];
let stdoutBuf = Buffer.alloc(0);
let protocolError = null;

child.stdout.on("data", (chunk) => {
  stdoutBuf = Buffer.concat([stdoutBuf, chunk]);
  while (stdoutBuf.length >= 4) {
    const len = stdoutBuf.readUInt32LE(0);
    if (len > 1024 * 1024) {
      protocolError = `invalid frame length ${len} (stdout corrupted?)`;
      return;
    }
    if (stdoutBuf.length < 4 + len) return;
    const text = stdoutBuf.subarray(4, 4 + len).toString("utf8");
    stdoutBuf = stdoutBuf.subarray(4 + len);
    let msg;
    try {
      msg = JSON.parse(text);
    } catch {
      protocolError = `non-JSON frame: ${text.slice(0, 80)}`;
      continue;
    }
    received.push(msg);
    console.log("<-", JSON.stringify(msg));
    for (const w of [...waiters]) {
      if (w.pred(msg)) {
        waiters.splice(waiters.indexOf(w), 1);
        w.resolve(msg);
      }
    }
  }
});
child.stderr.on("data", (d) => process.stderr.write(`   [companion] ${d}`));

function send(msg) {
  const body = Buffer.from(JSON.stringify(msg), "utf8");
  const header = Buffer.alloc(4);
  header.writeUInt32LE(body.length, 0);
  child.stdin.write(Buffer.concat([header, body]));
  console.log("->", JSON.stringify(msg));
}

function waitFor(pred, ms, label) {
  return new Promise((resolve, reject) => {
    const w = { pred, resolve };
    waiters.push(w);
    setTimeout(() => {
      const i = waiters.indexOf(w);
      if (i >= 0) {
        waiters.splice(i, 1);
        reject(new Error(`timeout waiting for ${label}`));
      }
    }, ms);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (cond, label) => {
  console.log(`${cond ? "PASS" : "FAIL"}: ${label}`);
  if (!cond) failures.push(label);
};

function playSpeech(text) {
  return new Promise((resolve) => {
    const ps = spawn("powershell.exe", [
      "-NoProfile",
      "-Command",
      `Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.Speak("${text}")`,
    ]);
    ps.on("exit", resolve);
  });
}

try {
  send({ type: "status" });
  const st = await waitFor((m) => m.type === "status", 5000, "initial status");
  check(st.running === false && st.state === "stopped", "initial status is stopped");

  send({ type: "stop" });
  await waitFor((m) => m.type === "stopped", 3000, "stopped (stop while stopped)");
  check(true, "stop while stopped returns stopped");

  send({ type: "bogus" });
  const inv = await waitFor((m) => m.type === "error", 3000, "invalid command error");
  check(inv.code === "INVALID_COMMAND", "unknown command returns INVALID_COMMAND");

  send({ type: "start" });
  send({ type: "start" });
  const startOutcome = await waitFor((m) => m.type === "started" || m.type === "error", 20000, "start outcome");

  if (startOutcome.type === "error") {
    console.log(`start returned error ${startOutcome.code}: ${startOutcome.message}`);
    check(typeof startOutcome.message === "string" && !/panicked|backtrace/i.test(startOutcome.message), "error message is clean");
  } else {
    check(startOutcome.sampleRate > 0 && startOutcome.channels === 1 && !!startOutcome.device, "started has device/sampleRate/channels");
    const dupStatus = received.find((m) => m.type === "status" && m.state === "starting");
    check(!!dupStatus, "second start while starting returns status (no second capture)");

    send({ type: "start" });
    const again = await waitFor((m) => m.type === "status", 3000, "status for start while listening");
    check(again.running === true && again.state === "listening", "start while listening returns current status");

    if (speak) {
      await sleep(500);
      await playSpeech("Hello and welcome to today's video. This sentence is played through the speakers.");
      await sleep(2500);
    } else {
      await sleep(3000);
    }

    send({ type: "status" });
    const live = await waitFor((m) => m.type === "status" && m.state === "listening", 3000, "listening status");
    check(live.deepgramConnected === true, "status reports deepgramConnected while listening");

    send({ type: "stop" });
    send({ type: "stop" });
    await waitFor((m) => m.type === "stopped", 8000, "stopped");
    check(true, "stop returns stopped");

    send({ type: "status" });
    const after = await waitFor((m) => m.type === "status" && m.state === "stopped", 3000, "status after stop");
    check(after.running === false && after.deepgramConnected === false, "status after stop is idle");

    if (speak) {
      const finals = received.filter((m) => m.type === "transcript_final");
      const partials = received.filter((m) => m.type === "transcript_partial");
      check(finals.length > 0, `received transcript_final (${finals.length}) and partial (${partials.length})`);
    }
  }

  check(received.every((m) => !("audio" in m) && !("pcm" in m)), "no audio payloads sent to the extension");
  check(protocolError === null, `stdout carried only valid frames${protocolError ? ` (${protocolError})` : ""}`);

  child.stdin.end();
  const code = await new Promise((resolve) => {
    const t = setTimeout(() => {
      child.kill();
      resolve("killed (did not exit on EOF)");
    }, 8000);
    child.on("exit", (c) => {
      clearTimeout(t);
      resolve(c);
    });
  });
  check(code === 0, `companion exits cleanly when Chrome closes stdin (exit ${code})`);
} catch (e) {
  failures.push(String(e));
  console.log("FAIL:", e.message);
  child.kill();
}

console.log(failures.length ? `\n${failures.length} check(s) failed` : "\nAll checks passed");
process.exit(failures.length ? 1 : 0);
