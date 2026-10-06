<#
.SYNOPSIS
    Builds the companion and the extension and creates the release zip:
    release\SystemAudioTranscriber-<version>.zip

.PARAMETER SkipBuild
    Package the existing build outputs without rebuilding.
#>
[CmdletBinding()]
param(
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'

$Root = $PSScriptRoot
$CompanionDir = Join-Path $Root 'native-companion'
$ExtensionSrc = Join-Path $Root 'chrome-extension'
$Version = (Get-Content -LiteralPath (Join-Path $ExtensionSrc 'manifest.json') -Raw | ConvertFrom-Json).version
$ReleaseDir = Join-Path $Root 'release'
$Stage = Join-Path $ReleaseDir 'SystemAudioTranscriber'
$Zip = Join-Path $ReleaseDir "SystemAudioTranscriber-$Version.zip"

function Invoke-Checked([string] $Dir, [scriptblock] $Command, [string] $Label) {
    Push-Location $Dir
    try {
        & $Command
        if ($LASTEXITCODE -ne 0) { throw "$Label failed (exit $LASTEXITCODE)." }
    }
    finally {
        Pop-Location
    }
}

if (-not $SkipBuild) {
    Write-Host "Building companion..." -ForegroundColor Cyan
    Invoke-Checked $CompanionDir { cargo build --release } 'cargo build'

    Write-Host "Building extension..." -ForegroundColor Cyan
    if (-not (Test-Path -LiteralPath (Join-Path $ExtensionSrc 'node_modules'))) {
        Invoke-Checked $ExtensionSrc { npm ci --no-audit --no-fund } 'npm ci'
    }
    Invoke-Checked $ExtensionSrc { npm run build } 'npm run build'
}

$Exe = Join-Path $CompanionDir 'target\release\system-audio-companion.exe'
if (-not (Test-Path -LiteralPath $Exe)) { throw "Companion not built: $Exe" }
$ascii = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($Exe))
if ($ascii -match 'VCRUNTIME140\.dll') {
    throw "The companion still depends on VCRUNTIME140.dll; check native-companion\.cargo\config.toml."
}

Write-Host "Assembling package..." -ForegroundColor Cyan
if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
$null = New-Item -ItemType Directory -Force -Path `
    (Join-Path $Stage 'extension\dist'), (Join-Path $Stage 'companion'), (Join-Path $Stage 'installer')

foreach ($file in 'manifest.json', 'popup.html', 'popup.css') {
    Copy-Item -LiteralPath (Join-Path $ExtensionSrc $file) -Destination (Join-Path $Stage 'extension')
}
Copy-Item -Path (Join-Path $ExtensionSrc 'dist\*.js') -Destination (Join-Path $Stage 'extension\dist')
Copy-Item -LiteralPath $Exe -Destination (Join-Path $Stage 'companion')
foreach ($file in 'install.ps1', 'uninstall.ps1') {
    Copy-Item -LiteralPath (Join-Path $CompanionDir "installer\$file") -Destination (Join-Path $Stage 'installer')
}
foreach ($file in 'Setup.cmd', 'Uninstall.cmd', 'setup.ps1', 'README.txt') {
    Copy-Item -LiteralPath (Join-Path $Root "packaging\$file") -Destination $Stage
}

if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
Compress-Archive -Path $Stage -DestinationPath $Zip

$size = [math]::Round((Get-Item -LiteralPath $Zip).Length / 1MB, 1)
Write-Host ""
Write-Host "Created $Zip ($size MB)" -ForegroundColor Green
Write-Host "Copy it to the other laptop, extract it, and run Setup.cmd."
