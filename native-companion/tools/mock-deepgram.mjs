// Minimal local stand-in for the Deepgram live endpoint, for testing the companion
// without a real API key. Dependency-free (Node >= 18).
//
//   node tools/mock-deepgram.mjs [port]
//   $env:SYSTEM_AUDIO_DEEPGRAM_ENDPOINT = "ws://127.0.0.1:8765/v1/listen"
//
// Behaviour: emits a partial every ~1 s of non-silent audio and a final every ~3 s,
// flushes a final on Finalize/CloseStream, then closes like Deepgram does.

import { createServer } from "node:http";
import { createHash } from "node:crypto";

const port = Number(process.argv[2] ?? 8765);
const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

function frame(opcode, payload) {
  const len = payload.length;
  let header;
  if (len < 126) {
    header = Buffer.from([0x80 | opcode, len]);
  } else if (len < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 126;
    header.writeUInt16BE(len, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(len), 2);
  }
  return Buffer.concat([header, payload]);
}

const results = (text, isFinal) =>
  JSON.stringify({
    type: "Results",
    is_final: isFinal,
    speech_final: isFinal,
    channel: { alternatives: [{ transcript: text, confidence: 0.99, words: [] }] },
  });

const server = createServer((_req, res) => {
  res.writeHead(426);
  res.end();
});

server.on("upgrade", (req, socket) => {
  const url = new URL(req.url, "http://localhost");
  const auth = req.headers["authorization"] ?? "";
  if (!auth.startsWith("Token ") || auth.length <= 6) {
    socket.end("HTTP/1.1 401 Unauthorized\r\ndg-error: Invalid credentials.\r\n\r\n");
    console.error("[mock] rejected connection without Token auth");
    return;
  }
  const accept = createHash("sha1").update(req.headers["sec-websocket-key"] + GUID).digest("base64");
  socket.write(
    `HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  const params = Object.fromEntries(url.searchParams);
  console.error(`[mock] connected ${url.pathname} ${JSON.stringify(params)}`);

  const bytesPerSecond = Number(params.sample_rate ?? 48000) * 2;
  let buf = Buffer.alloc(0);
  let audioBytes = 0;
  let speechBytes = 0;
  let sinceFinal = 0;
  let utterance = 0;
  let words = [];
  let keepAlives = 0;
  let closed = false;

  const sendText = (s) => !closed && socket.write(frame(0x1, Buffer.from(s)));
  const flushFinal = () => {
    if (words.length) {
      sendText(results(`Mock final sentence ${++utterance}: ${words.join(" ")}.`, true));
      words = [];
    }
    sinceFinal = 0;
  };
  const close = () => {
    if (closed) return;
    sendText(JSON.stringify({ type: "Metadata", request_id: "mock" }));
    const payload = Buffer.alloc(2);
    payload.writeUInt16BE(1000, 0);
    socket.write(frame(0x8, payload));
    closed = true;
    setTimeout(() => socket.end(), 100);
    console.error(
      `[mock] closed: audio=${(audioBytes / bytesPerSecond).toFixed(1)}s speech=${(speechBytes / bytesPerSecond).toFixed(1)}s keepalives=${keepAlives} finals=${utterance}`,
    );
  };

  socket.on("data", (data) => {
    buf = Buffer.concat([buf, data]);
    while (buf.length >= 2) {
      const opcode = buf[0] & 0x0f;
      const masked = (buf[1] & 0x80) !== 0;
      let len = buf[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        off = 10;
      }
      const maskLen = masked ? 4 : 0;
      if (buf.length < off + maskLen + len) return;
      const mask = masked ? buf.subarray(off, off + 4) : null;
      const payload = Buffer.from(buf.subarray(off + maskLen, off + maskLen + len));
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
      buf = buf.subarray(off + maskLen + len);

      if (opcode === 0x2) {
        audioBytes += payload.length;
        if (payload.some((b) => b !== 0)) {
          const before = Math.floor(speechBytes / bytesPerSecond);
          speechBytes += payload.length;
          sinceFinal += payload.length;
          if (Math.floor(speechBytes / bytesPerSecond) > before) {
            words.push(`word${words.length + 1}`);
            sendText(results(words.join(" "), false));
          }
          if (sinceFinal >= bytesPerSecond * 3) flushFinal();
        }
      } else if (opcode === 0x1) {
        const msg = JSON.parse(payload.toString());
        if (msg.type === "KeepAlive") keepAlives++;
        else if (msg.type === "Finalize") flushFinal();
        else if (msg.type === "CloseStream") {
          flushFinal();
          close();
        }
      } else if (opcode === 0x8) {
        if (!closed) close();
      }
    }
  });
  socket.on("error", () => {});
});

server.listen(port, "127.0.0.1", () => console.error(`[mock] Deepgram mock on ws://127.0.0.1:${port}/v1/listen`));
