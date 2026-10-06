<#
.SYNOPSIS
    Installs the System Audio Companion for the current user and registers it as a
    Chrome Native Messaging host (HKCU, no administrator rights needed).

.PARAMETER ExtensionId
    The 32-character ID of the extension shown on chrome://extensions.

.PARAMETER InstallDir
    Where to install. Defaults to %LOCALAPPDATA%\SystemAudioCompanion.

.PARAMETER SourceExe
    Release executable to install. Defaults to ..\target\release\system-audio-companion.exe.

.PARAMETER Build
    Run `cargo build --release` before installing.

.EXAMPLE
    .\install.ps1 -ExtensionId abcdefghijklmnopabcdefghijklmnop -Build
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[a-p]{32}$')]
    [string] $ExtensionId,

    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'SystemAudioCompanion'),

    [string] $SourceExe,

    [switch] $Build
)

$ErrorActionPreference = 'Stop'

$HostName = 'com.systemaudio.deepgram'
$ExeName = 'system-audio-companion.exe'
$ManifestName = 'native-host-manifest.json'
$RegistryKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
$CompanionRoot = Split-Path -Parent $PSScriptRoot

if (-not $SourceExe) {
    $SourceExe = Join-Path $CompanionRoot "target\release\$ExeName"
}

if ($Build -or -not (Test-Path -LiteralPath $SourceExe)) {
    Write-Host "Building release executable..."
    Push-Location $CompanionRoot
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed (exit $LASTEXITCODE)." }
    }
    finally {
        Pop-Location
    }
}
if (-not (Test-Path -LiteralPath $SourceExe)) {
    throw "Release executable not found: $SourceExe"
}

# 1. Application directory
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$InstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
$ExePath = Join-Path $InstallDir $ExeName
$ManifestPath = Join-Path $InstallDir $ManifestName

# A running companion (started by Chrome) locks the exe; stop only our installed copy.
Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($ExeName)) -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $ExePath } |
    ForEach-Object {
        Write-Host "Stopping running companion (PID $($_.Id))..."
        Stop-Process -Id $_.Id -Force
        $_.WaitForExit(5000) | Out-Null
    }

# 2. Copy release EXE
Copy-Item -LiteralPath $SourceExe -Destination $ExePath -Force

# 3. Native Messaging manifest with the real installed path (UTF-8 without BOM)
$manifest = [ordered]@{
    name            = $HostName
    description     = 'System Audio Deepgram Companion'
    path            = $ExePath
    type            = 'stdio'
    allowed_origins = @("chrome-extension://$ExtensionId/")
}
$json = $manifest | ConvertTo-Json -Depth 4
[IO.File]::WriteAllText($ManifestPath, $json, (New-Object System.Text.UTF8Encoding($false)))

# 4/5. Register the host; the key's default value points at the manifest
New-Item -Path $RegistryKey -Force | Out-Null
Set-Item -Path $RegistryKey -Value $ManifestPath

# 6. Verify
$problems = @()
if (-not (Test-Path -LiteralPath $ExePath)) { $problems += "Executable missing: $ExePath" }
if (-not (Test-Path -LiteralPath $ManifestPath)) { $problems += "Manifest missing: $ManifestPath" }
$registered = (Get-Item -Path $RegistryKey).GetValue('')
if ($registered -ne $ManifestPath) { $problems += "Registry default value is '$registered', expected '$ManifestPath'" }
$parsed = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
if ($parsed.path -ne $ExePath) { $problems += "Manifest path mismatch: $($parsed.path)" }
if (@($parsed.allowed_origins).Count -ne 1 -or @($parsed.allowed_origins)[0] -ne "chrome-extension://$ExtensionId/") {
    $problems += "Manifest allowed_origins is not exactly chrome-extension://$ExtensionId/"
}
if ($problems.Count -gt 0) {
    $problems | ForEach-Object { Write-Error $_ -ErrorAction Continue }
    throw "Installation verification failed."
}

# 7. Summary
$keyConfigured = [bool]$env:DEEPGRAM_API_KEY -or
    [bool][Environment]::GetEnvironmentVariable('DEEPGRAM_API_KEY', 'User')

Write-Host ""
Write-Host "System Audio Companion installed." -ForegroundColor Green
Write-Host "  Executable : $ExePath"
Write-Host "  Manifest   : $ManifestPath"
Write-Host "  Registry   : $RegistryKey"
Write-Host "  Allowed    : chrome-extension://$ExtensionId/"
Write-Host "  Logs       : $(Join-Path $InstallDir 'logs')"
Write-Host ""
if ($keyConfigured) {
    Write-Host "DEEPGRAM_API_KEY: configured" -ForegroundColor Green
}
else {
    Write-Host "DEEPGRAM_API_KEY: NOT configured. Set it for your user account:" -ForegroundColor Yellow
    Write-Host '  [Environment]::SetEnvironmentVariable("DEEPGRAM_API_KEY", (Read-Host "Deepgram API key"), "User")'
}
Write-Host ""
Write-Host "Reload the extension (or restart Chrome), open its side panel and click Start Listening."
