# System Audio Transcriber

Live transcript of **Windows system playback audio** (YouTube, Zoom/Meet remote
participants, Spotify, notifications, ...) shown in a Chrome extension.

```text
Windows audio engine -> WASAPI loopback (default RENDER endpoint)
  -> system-audio-companion.exe (PCM16 mono) -> Deepgram live STT
  -> transcript text -> Chrome Native Messaging -> extension side panel
```

- Captures **system output only**: the default render endpoint (`eRender`) opened with
  `AUDCLNT_STREAMFLAGS_LOOPBACK`. No microphone or capture endpoint is ever enumerated
  or opened, and the extension requests no microphone, screen, or tab-capture permission.
- Raw audio goes only to Deepgram. The extension receives only transcript, status, and
  error JSON.
- Audio is never written to disk.
- **Capture** (side panel) is done by the companion with Windows GDI, so Chrome
  shows no screen picker. The PNG stays local (Downloads + clipboard) and is never sent to
  the extension or the network.

## Repository layout

```text
chrome-extension/          MV3 extension (TypeScript, built with tsc)
  manifest.json            nativeMessaging, sidePanel, scripting + <all_urls> (for typing)
  popup.html / popup.css   side panel page
  src/background.ts        Native Messaging port, transcript state, bounded reconnect,
                           forwarding to the locked tab
  src/popup.ts             UI
  src/content.ts           types the live transcript into the focused text box
  src/messages.ts          shared message types
native-companion/          Rust companion (Windows 11)
  src/audio.rs             WASAPI loopback capture (eRender + LOOPBACK)
  src/convert.rs           downmix -> PCM16, 100 ms chunks, bounded channel
  src/deepgram.rs          DeepgramConfig (model etc.), WebSocket streaming, parsing
  src/messaging.rs         4-byte LE length + JSON framing on stdin/stdout
  src/protocol.rs          command/response types
  src/state.rs             Stopped/Starting/Listening/Stopping/Error state machine
  src/devtools.rs          --meter, --pcm-test, --transcribe-stderr
  src/screen.rs            primary-display capture, 10% top/bottom crop, PNG, clipboard
  installer/               install.ps1, uninstall.ps1, manifest template
  tools/                   test harnesses (Native Messaging, mock Deepgram, Chrome E2E)
  .cargo/config.toml       static C runtime (no VC++ Redistributable needed)
packaging/                 Setup.cmd, setup.ps1, Uninstall.cmd, README.txt for the zip
package.ps1                builds everything into release\SystemAudioTranscriber-<version>.zip
```

## Install on another laptop (no build tools)

On the development machine, build the release zip:

```powershell
powershell -ExecutionPolicy Bypass -File .\package.ps1
```

This creates `release\SystemAudioTranscriber-<version>.zip` (about 0.5 MB). On the other
laptop (Windows 10/11 + Chrome, no admin rights, Rust, Node.js or Visual C++ runtime
needed): extract it, double-click **Setup.cmd**, paste the Deepgram key when asked, then on
`chrome://extensions` enable **Developer mode**, click **Load unpacked** and choose
`%LOCALAPPDATA%\SystemAudioCompanion\extension` (Setup copies the path to the clipboard).
**Uninstall.cmd** removes everything. Chrome only allows Web Store extensions to be
installed automatically, so Load unpacked is the one manual step.

The extension ID is fixed (`foiceiaejcejkobbkhekacfpebekndpj`) by the `key` in
`manifest.json`, so it is the same on every machine and whatever folder it is loaded from.
The matching private key is kept outside the repo
(`%USERPROFILE%\.system-audio-transcriber\extension-key.pem`); it is only needed to publish
to the Chrome Web Store.

## Prerequisites

- Windows 11, Google Chrome
- Rust (stable, MSVC toolchain)
- Node.js 18+ (to build the extension and run the test tools)
- One Deepgram API key

## Setup

### 1. Build

```powershell
cd native-companion
cargo build --release

cd ..\chrome-extension
npm install
npm run build
```

### 2. Load the extension

1. Open `chrome://extensions` and enable **Developer mode**.
2. Click **Load unpacked** and select the `chrome-extension` folder.
3. The extension ID is always `foiceiaejcejkobbkhekacfpebekndpj` (pinned by the `key` in
   `manifest.json`).

### 3. Install the companion (current user, no admin rights)

```powershell
cd native-companion\installer
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

`-ExtensionId <id>` overrides the fixed ID (the Chrome E2E tool uses this).

This copies the exe to `%LOCALAPPDATA%\SystemAudioCompanion`, writes
`native-host-manifest.json` with the real exe path and exactly
`chrome-extension://<id>/` as the allowed origin, and registers
`HKCU\Software\Google\Chrome\NativeMessagingHosts\com.systemaudio.deepgram`.
Add `-Build` to rebuild first, or `-InstallDir` to choose another folder.
Re-run the script after every rebuild to update the installed exe.

### 4. Configure the Deepgram key

The companion reads `DEEPGRAM_API_KEY`. The key is never written to any project file,
manifest, log, or stdout, and it is never sent to the extension.

Current PowerShell session only (developer modes):

```powershell
$env:DEEPGRAM_API_KEY = "<your key>"
```

Persistent, for your Windows user (recommended). This prompts for the key so it does
not end up in your shell history:

```powershell
[Environment]::SetEnvironmentVariable("DEEPGRAM_API_KEY", (Read-Host "Deepgram API key"), "User")
```

Chrome starts the companion and passes it Chrome's own environment. The companion also
reads the persistent user value (`HKCU\Environment`) directly, so a newly set key works
without restarting Chrome.

To remove it:

```powershell
[Environment]::SetEnvironmentVariable("DEEPGRAM_API_KEY", $null, "User")
```

### 5. Use it

Click the extension's toolbar icon to open it in Chrome's side panel, then click
**Start Listening**. The panel shows **Listening**
while capture is active. Partial results appear in grey italics and are replaced by the
final text. The transcript is split into blocks at pauses of 3 seconds or more, each
stamped with the time since you first clicked Start (for example `[1:30]`); the clock keeps
running across Stop/Start and restarts at `[0:00]` when you click **Clear**. Click **Stop Listening** to stop capture immediately and close the
Deepgram connection. The transcript is kept in memory until you click **Clear** or Chrome
closes.

#### Type the transcript into a page

Click into a text box on any page (Google Docs, a chat box, a form field), then click
**Lock** at the top of the side panel. The button turns blue (**Locked**) and pulses while
text is flowing; it turns amber if there is no text box to type into (hover it to see why).
While listening, the grey in-progress text is typed into that box as it arrives and
corrected in place whenever Deepgram revises it, so the box always matches the panel.
Finished sentences are committed and the next one starts after them. The locked tab keeps
receiving text when you switch to another tab or app. Click **Locked** again to unlock:
unlock stops typing immediately (and drops the grey in-progress text from the box), while
listening can continue; lock again to catch up any finished sentences you missed, then
resume live typing.

If you type or move the caret in the box while it is live, your edits are kept and the
transcript continues from the caret. Chrome doesn't allow extensions on `chrome://` pages
or the Chrome Web Store.

#### Capture the screen

Click **Capture** at the top of the side panel (it spins while working, then flashes green
**Saved** or red **Failed**; hover for details). The companion captures the primary display at
its full physical resolution, crops off the top 10% and bottom 10% (1920x1080 becomes
1920x864), saves `Screenshot YYYY-MM-DD HHMMSS.png` to your Downloads folder and copies the
image to the clipboard, ready to paste with Ctrl+V. It works whether or not you are
listening.

## Developer modes

Run these from a terminal. Their output goes to stderr.

```powershell
cd native-companion
.\target\release\system-audio-companion.exe --meter              # live level meter
.\target\release\system-audio-companion.exe --pcm-test           # PCM16 conversion stats
.\target\release\system-audio-companion.exe --transcribe-stderr  # Deepgram transcripts
.\target\release\system-audio-companion.exe --capture-screen     # cropped screenshot to Downloads + clipboard
```

Each mode accepts `--seconds N`.

## Automated checks

```powershell
cd native-companion
cargo test --release                                  # conversion, parsing, framing, protocol

node tools/nm-test.mjs --speak                        # Native Messaging protocol, like Chrome

# Full pipeline without a real key, using the local mock Deepgram server:
Start-Process node "tools/mock-deepgram.mjs 8765"
$env:SYSTEM_AUDIO_DEEPGRAM_ENDPOINT = "ws://127.0.0.1:8765/v1/listen"
$env:DEEPGRAM_API_KEY = "mock"
node tools/nm-test.mjs --speak
node tools/chrome-e2e.mjs --speak                     # headless Chrome clicks Start/Stop in the popup
Remove-Item Env:SYSTEM_AUDIO_DEEPGRAM_ENDPOINT, Env:DEEPGRAM_API_KEY
```

`--speak` plays Windows text-to-speech through the speakers while listening.
`chrome-e2e.mjs` loads the extension into a temporary profile and runs `install.ps1`
for that extension ID.

## Manual acceptance tests

| Test | Action | Expected |
| --- | --- | --- |
| 1 | Nothing playing, speak into the microphone | `--meter` stays at `░░░░░░░░░░`, no transcript |
| 2 | Play a spoken YouTube video | Meter moves, transcript appears |
| 3 | Pause YouTube | Meter drops to silence, no new transcript |
| 4 | Speak into the microphone while YouTube is paused | No transcript |
| 5 | Play Spotify | Meter moves (speech transcription depends on content) |
| 6 | Zoom or Meet call | Remote participant is transcribed, your microphone stays unopened |

No screen-share picker and no microphone permission prompt should appear at any point.
If an application deliberately plays your microphone back through the speakers, that
audio becomes system output and will be transcribed. This is expected.

## Native Messaging protocol

Extension to companion: `{"type":"start"}`, `{"type":"stop"}`, `{"type":"status"}`,
`{"type":"capture_screen"}`.

Companion to extension:

```json
{"type":"started","device":"Speakers (Realtek(R) Audio)","sampleRate":48000,"channels":1}
{"type":"stopped"}
{"type":"status","running":true,"state":"listening","deepgramConnected":true,"device":"Speakers (Realtek(R) Audio)"}
{"type":"transcript_partial","text":"Hello every"}
{"type":"transcript_final","text":"Hello everyone."}
{"type":"device_changed","device":"Headphones (USB Audio)"}
{"type":"screen_captured","path":"C:\\Users\\me\\Downloads\\Screenshot 2026-10-05 235234.png","copied":true,"width":1920,"height":864}
{"type":"error","code":"NO_API_KEY","message":"DEEPGRAM_API_KEY is not configured."}
```

Error codes: `NO_API_KEY`, `NO_OUTPUT_DEVICE`, `AUDIO_INIT_FAILED`, `AUDIO_DEVICE_LOST`,
`DEEPGRAM_AUTH_FAILED`, `DEEPGRAM_CONNECTION_FAILED`, `DEEPGRAM_DISCONNECTED`,
`INVALID_COMMAND`, `SCREEN_CAPTURE_FAILED`.

`start` and `stop` are idempotent. Sending `start` while listening returns the current
status, and sending `stop` while stopped returns `stopped`.

## Behaviour notes

- **Idle:** with no capture and no Deepgram connection, the companion process just waits
  on stdin.
- **Audio format:** the Windows mix format (typically 48 kHz stereo float32) is downmixed
  to mono PCM16 at the same sample rate, with no resampling. Deepgram is told the actual
  rate.
- **Silence:** after about 1 s of digital silence (for example, a paused video), the
  companion stops streaming audio and sends Deepgram `KeepAlive` messages, so you are not
  billed for silence. Streaming resumes as soon as sound plays.
- **Default device changes** (for example, plugging in headphones) are detected within
  about 2 s, and capture switches to the new default playback device.
- **Model:** the Deepgram model and options live only in `DeepgramConfig::default()` in
  `native-companion/src/deepgram.rs` (default `nova-3`, `en`).
- **Logs:** `%LOCALAPPDATA%\SystemAudioCompanion\logs\companion.log`, rotated at 5 MB.
  Logs never contain the API key, the authorization header, or audio.

## Troubleshooting

- **"System Audio Companion is not installed"** (`Specified native messaging host not
  found`): run `install.ps1` with the correct extension ID.
- **"Access to the specified native messaging host is forbidden"**: the extension ID
  changed (for example, the folder moved). Re-run `install.ps1` with the new ID.
- **`DEEPGRAM_AUTH_FAILED`**: the key is wrong or revoked.
- **Winsock layered service providers:** the companion talks to Deepgram over plain std
  sockets on a dedicated thread instead of tokio's socket driver. tokio's Windows socket
  driver crashes inside some old third-party Winsock LSPs, such as Astrill's
  `ASProxy64.dll`. tokio is still used for the controller and channels.

## Uninstall

```powershell
powershell -ExecutionPolicy Bypass -File native-companion\installer\uninstall.ps1
```

This removes the registry key, the installed exe and manifest, and the logs (add
`-KeepLogs` to keep them). It does not touch `DEEPGRAM_API_KEY`.
