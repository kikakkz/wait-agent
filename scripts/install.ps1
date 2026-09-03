# WaitAgent install script for Windows (PowerShell).
# Usage:
#   irm https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.ps1 | iex
# Optional: -Version 0.1.49  (default: latest release)

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:WAITAGENT_VERSION) { $env:WAITAGENT_VERSION } else { "latest" }),
    [string]$InstallDir = $(if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\waitagent" })
)

$ErrorActionPreference = "Stop"
$Repo = "kikakkz/wait-agent"
$ArchiveBase = "waitagent-{0}-x86_64-windows.zip"

# GitHub requires TLS 1.2 on Windows PowerShell 5.1.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$ProgressPreference = "SilentlyContinue"

function Resolve-Version {
    if ($Version -ne "latest") { return $Version.TrimStart("v") }
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "waitagent-installer" } -TimeoutSec 30
    if (-not $release.tag_name) { throw "failed to fetch latest release from GitHub API" }
    return $release.tag_name.TrimStart("v")
}

function Add-ToUserPath {
    param([string]$Dir)
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($null -eq $userPath) { $userPath = "" }
    $entries = $userPath -split ";" | Where-Object { $_ -ne "" }
    if ($entries -notcontains $Dir) {
        $newPath = if ($userPath.TrimEnd(";") -eq "") { $Dir } else { $userPath.TrimEnd(";") + ";" + $Dir }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Host ">>> Added $Dir to your user PATH (open a new terminal to pick it up)"
    }
    # Make it available in the current session too.
    if (($env:PATH -split ";") -notcontains $Dir) { $env:PATH = "$Dir;$env:PATH" }
}

$resolved = Resolve-Version
$archive = $ArchiveBase -f $resolved
$url = "https://github.com/$Repo/releases/download/v$resolved/$archive"

Write-Host ">>> WaitAgent $resolved for windows-x86_64"
Write-Host ">>> Downloading $url"

$tmpdir = Join-Path ([System.IO.Path]::GetTempPath()) ("waitagent-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmpdir | Out-Null
try {
    $zipPath = Join-Path $tmpdir $archive
    Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing -TimeoutSec 60

    Write-Host ">>> Extracting..."
    Expand-Archive -Path $zipPath -DestinationPath $tmpdir -Force

    $binary = Join-Path $tmpdir "waitagent.exe"
    $signalSender = Join-Path $tmpdir "waitagent-agent-signal-send.exe"
    if (-not (Test-Path $binary)) { throw "waitagent.exe not found in archive" }

    Write-Host ">>> Installing to $InstallDir"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Force $binary (Join-Path $InstallDir "waitagent.exe")
    if (Test-Path $signalSender) {
        Copy-Item -Force $signalSender (Join-Path $InstallDir "waitagent-agent-signal-send.exe")
    }

    $installed = Join-Path $InstallDir "waitagent.exe"
    $versionOutput = & $installed --version
    Write-Host "$versionOutput"
    if ($versionOutput -notmatch "waitagent $resolved ") {
        throw "installed binary reports unexpected version: $versionOutput"
    }

    Add-ToUserPath $InstallDir

    Write-Host ""
    Write-Host "waitagent $resolved installed to $InstallDir" -ForegroundColor Green
    Write-Host ""
    Write-Host "To get started (open a new terminal if this one does not see waitagent yet):"
    Write-Host "  waitagent --help"
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmpdir
}
