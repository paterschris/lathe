[CmdletBinding()]
Param(
    [Parameter()][string]$ArchivePath
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Set-Location (Join-Path $PSScriptRoot '..')

$channel = (Get-Content 'crates/zed/RELEASE_CHANNEL').Trim()
$suffix = if ($channel -ne 'stable') { "-$channel" } else { '' }

$appName = switch ($channel) {
    'stable'  { 'Lathe' }
    'preview' { 'Lathe Preview' }
    'beta'    { 'Lathe Beta' }
    'nightly' { 'Lathe Nightly' }
    default   { 'Lathe Dev' }
}

# --- Find archive ---
if (-not $ArchivePath) {
    $candidates = Get-ChildItem -Path 'target/release' -Filter 'Lathe-*-windows.zip' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending
    if (-not $candidates) {
        Write-Error "No Windows zip found in target/release/. Run 'script/build-fork-windows.ps1 && script/package-fork-windows.ps1' first."
        exit 1
    }
    $ArchivePath = $candidates[0].FullName
}

if (-not (Test-Path $ArchivePath)) {
    Write-Error "Archive not found: $ArchivePath"
    exit 1
}

# --- Install location ---
$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\Lathe'
$installDir  = Join-Path $installRoot "lathe$suffix"
$binDir      = Join-Path $installDir 'bin'

Write-Output "=== Installing $appName ==="
Write-Output "  From: $ArchivePath"
Write-Output "  To:   $installDir"

# --- Detect a running instance and ask the user before overwriting ---
$running = Get-Process -Name 'lathe-editor', 'lathe' -ErrorAction SilentlyContinue
if ($running) {
    Write-Output ""
    Write-Output "An existing Lathe process is running. Close it before installing, or it will be terminated."
    $reply = Read-Host "Terminate running Lathe processes? [y/N]"
    if ($reply -match '^[Yy]') {
        $running | Stop-Process -Force
        Start-Sleep -Seconds 1
    } else {
        Write-Error "Install aborted."
        exit 1
    }
}

# --- Clean previous install ---
if (Test-Path $installDir) {
    Write-Output "Removing existing installation..."
    Remove-Item -Path $installDir -Recurse -Force
}
New-Item -ItemType Directory -Path $installDir -Force | Out-Null

# --- Extract via temp dir, then move contents into installDir ---
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
try {
    Expand-Archive -Path $ArchivePath -DestinationPath $tempDir -Force

    # The zip wraps everything in `lathe${suffix}/` -- flatten one level.
    $inner = Join-Path $tempDir "lathe$suffix"
    if (-not (Test-Path $inner)) {
        Write-Error "Archive layout unexpected: missing 'lathe$suffix/' at root of $ArchivePath"
        exit 1
    }
    Get-ChildItem -Path $inner -Force | ForEach-Object {
        Move-Item -Path $_.FullName -Destination $installDir -Force
    }
}
finally {
    Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

# --- Add bin/ to user PATH (idempotent) ---
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = if ($userPath) { $userPath -split ';' } else { @() }
if ($pathEntries -notcontains $binDir) {
    $newPath = if ($userPath) { "$userPath;$binDir" } else { $binDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Output "Added $binDir to user PATH (open a new terminal to pick it up)."
} else {
    Write-Output "$binDir already on user PATH."
}

# --- Start Menu shortcut ---
$startMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcutPath = Join-Path $startMenuDir "$appName.lnk"
$editorExe = Join-Path $installDir 'libexec\lathe-editor.exe'
$iconPath = Join-Path $installDir 'share\lathe.ico'

try {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $editorExe
    $shortcut.WorkingDirectory = $installDir
    if (Test-Path $iconPath) { $shortcut.IconLocation = $iconPath }
    $shortcut.Save()
    Write-Output "Created Start Menu shortcut: $shortcutPath"
} catch {
    Write-Output "Warning: Could not create Start Menu shortcut: $_"
}

Write-Output ""
Write-Output "=== Installed ==="
Write-Output "  App: $installDir"
Write-Output "  CLI: $binDir\lathe.exe"
Write-Output ""
Write-Output "Launch from Start Menu, or open a new terminal and run:"
Write-Output "  lathe ."
Write-Output ""
Write-Output "Note: This fork installs as '$appName' and runs alongside stock Zed."
