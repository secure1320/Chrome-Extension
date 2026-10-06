@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0installer\uninstall.ps1"
echo.
echo Also remove "System Audio Transcriber" on chrome://extensions.
echo The DEEPGRAM_API_KEY user environment variable was left in place.
echo.
pause
