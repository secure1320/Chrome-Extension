<#
.SYNOPSIS
    Removes everything install.ps1 and the setup package created: the HKCU Native
    Messaging registration, the installed executable and manifest, the installed
    extension folder, and (unless -KeepLogs) the log folder.
    The DEEPGRAM_API_KEY environment variable is left untouched.
#>
[CmdletBinding()]
param(
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'SystemAudioCompanion'),
    [switch] $KeepLogs
)

$ErrorActionPreference = 'Stop'

$HostName = 'com.systemaudio.deepgram'
$ExeName = 'system-audio-companion.exe'
$RegistryKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
$ExePath = Join-Path $InstallDir $ExeName
$ManifestPath = Join-Path $InstallDir 'native-host-manifest.json'
$LogDir = Join-Path $InstallDir 'logs'
$ExtensionDir = Join-Path $InstallDir 'extension'

if (Test-Path -Path $RegistryKey) {
    Remove-Item -Path $RegistryKey -Recurse -Force
    Write-Host "Removed registry key $RegistryKey"
}

Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($ExeName)) -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $ExePath } |
    ForEach-Object {
        Write-Host "Stopping running companion (PID $($_.Id))..."
        Stop-Process -Id $_.Id -Force
        $_.WaitForExit(5000) | Out-Null
    }

foreach ($file in @($ExePath, $ManifestPath)) {
    if (Test-Path -LiteralPath $file) {
        Remove-Item -LiteralPath $file -Force
        Write-Host "Removed $file"
    }
}

if (Test-Path -LiteralPath $ExtensionDir) {
    Remove-Item -LiteralPath $ExtensionDir -Recurse -Force
    Write-Host "Removed $ExtensionDir (also remove the extension on chrome://extensions)"
}

if (-not $KeepLogs -and (Test-Path -LiteralPath $LogDir)) {
    Remove-Item -LiteralPath $LogDir -Recurse -Force
    Write-Host "Removed $LogDir"
}

if ((Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
    Remove-Item -LiteralPath $InstallDir -Force
    Write-Host "Removed $InstallDir"
}

Write-Host "System Audio Companion uninstalled." -ForegroundColor Green
