System Audio Transcriber
========================

Live transcript of Windows playback audio in a Chrome side panel, typing into any
text box, and one-click screen capture.

Requirements: Windows 10 or 11, Google Chrome, a Deepgram API key.
No administrator rights, Rust, Node.js or Visual C++ runtime needed.

Install
-------
1. Extract this zip anywhere.
2. Double-click Setup.cmd. Paste your Deepgram API key when asked.
3. In Chrome (chrome://extensions opens automatically):
   - turn on "Developer mode" (top right),
   - click "Load unpacked" and choose the folder Setup shows
     (%LOCALAPPDATA%\SystemAudioCompanion\extension, already on your clipboard).
4. Pin the extension and click its icon to open the side panel.

You can delete the extracted zip folder afterwards.

Update
------
Extract the new zip, run its Setup.cmd, then click reload on the extension's card
on chrome://extensions.

Uninstall
---------
Run Uninstall.cmd, then remove the extension on chrome://extensions.

Logs: %LOCALAPPDATA%\SystemAudioCompanion\logs
