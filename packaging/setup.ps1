<#
.SYNOPSIS
    One-click setup from the release zip (run through Setup.cmd). Installs the
    companion for the current user, registers it with Chrome, copies the extension
    to a permanent folder, asks for the Deepgram key if it isn't set, and opens
    chrome://extensions for the one manual step (Load unpacked).

.PARAMETER NoLaunch
    Skip opening Chrome and Explorer (for automated tests).
#>
[CmdletBinding()]
param(
    [switch] $NoLaunch
)

$ErrorActionPreference = 'Stop'

$InstallDir = Join-Path $env:LOCALAPPDATA 'SystemAudioCompanion'
$ExtensionDir = Join-Path $InstallDir 'extension'

Write-Host "Installing System Audio Transcriber..." -ForegroundColor Cyan

# Files extracted from a downloaded zip carry the "downloaded from the internet" mark.
Get-ChildItem -LiteralPath $PSScriptRoot -Recurse -File | Unblock-File

# 1. Companion + Chrome Native Messaging registration (fixed extension ID).
& (Join-Path $PSScriptRoot 'installer\install.ps1') `
    -SourceExe (Join-Path $PSScriptRoot 'companion\system-audio-companion.exe') `
    -InstallDir $InstallDir

# 2. Extension in a permanent folder, so the zip can be deleted afterwards.
if (Test-Path -LiteralPath $ExtensionDir) {
    Remove-Item -LiteralPath $ExtensionDir -Recurse -Force
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'extension') -Destination $ExtensionDir -Recurse
Write-Host "Extension folder: $ExtensionDir"

# 3. Deepgram key, only if this Windows user doesn't have one yet.
if (-not [Environment]::GetEnvironmentVariable('DEEPGRAM_API_KEY', 'User')) {
    Write-Host ""
    $secure = Read-Host "Paste your Deepgram API key (input is hidden; press Enter to skip)" -AsSecureString
    $key = [System.Net.NetworkCredential]::new('', $secure).Password.Trim()
    if ($key) {
        [Environment]::SetEnvironmentVariable('DEEPGRAM_API_KEY', $key, 'User')
        Write-Host "DEEPGRAM_API_KEY saved for your Windows user." -ForegroundColor Green
    }
    else {
        Write-Host "Skipped. Run Setup.cmd again later to add the key." -ForegroundColor Yellow
    }
}

# 4. The one step Chrome requires a person to do.
if (-not $NoLaunch) {
    Set-Clipboard -Value $ExtensionDir
    try { Start-Process 'chrome.exe' 'chrome://extensions' } catch { Write-Host "Open chrome://extensions in Chrome." }
    Start-Process 'explorer.exe' $ExtensionDir
}

Write-Host ""
Write-Host "Last step (first time only):" -ForegroundColor Cyan
Write-Host "  1. On chrome://extensions, turn on 'Developer mode' (top right)."
Write-Host "  2. Click 'Load unpacked' and choose this folder (the path is on your clipboard):"
Write-Host "       $ExtensionDir"
Write-Host "  3. Pin the extension from the puzzle-piece menu and click it to open the side panel."
Write-Host ""
Write-Host "Already loaded before? Just click the reload button on the extension's card."
