[CmdletBinding()]
Param(
    [Parameter()][string]$Channel,
    [Parameter()][switch]$All
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

# If invoked from inside the repo, default to its RELEASE_CHANNEL. Otherwise
# require -Channel or -All so we don't guess which install to remove.
if (-not $Channel -and -not $All) {
    $repoChannel = Join-Path $PSScriptRoot '..\crates\zed\RELEASE_CHANNEL'
    if (Test-Path $repoChannel) {
        $Channel = (Get-Content $repoChannel).Trim()
    } else {
        Write-Error "Specify -Channel <stable|preview|beta|nightly|dev> or -All"
        exit 1
    }
}

$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\Lathe'
$startMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'

$channels = if ($All) {
    if (Test-Path $installRoot) {
        Get-ChildItem -Path $installRoot -Directory | ForEach-Object {
            if ($_.Name -eq 'lathe') { 'stable' }
            elseif ($_.Name -match '^lathe-(.+)$') { $matches[1] }
        }
    } else { @() }
} else {
    @($Channel)
}

if (-not $channels) {
    Write-Output "Nothing to uninstall."
    exit 0
}

# --- Stop any running Lathe processes ---
$running = Get-Process -Name 'lathe-editor', 'lathe' -ErrorAction SilentlyContinue
if ($running) {
    Write-Output "Stopping running Lathe processes..."
    $running | Stop-Process -Force
    Start-Sleep -Seconds 1
}

foreach ($ch in $channels) {
    $suffix = if ($ch -ne 'stable') { "-$ch" } else { '' }
    $installDir = Join-Path $installRoot "lathe$suffix"
    $binDir     = Join-Path $installDir 'bin'

    $appName = switch ($ch) {
        'stable'  { 'Lathe' }
        'preview' { 'Lathe Preview' }
        'beta'    { 'Lathe Beta' }
        'nightly' { 'Lathe Nightly' }
        default   { 'Lathe Dev' }
    }

    Write-Output "=== Uninstalling $appName ==="

    # --- Remove from user PATH ---
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath) {
        $pathEntries = $userPath -split ';' | Where-Object { $_ -and $_ -ne $binDir }
        $newPath = $pathEntries -join ';'
        if ($newPath -ne $userPath) {
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Output "  Removed $binDir from user PATH"
        }
    }

    # --- Remove install dir ---
    if (Test-Path $installDir) {
        Remove-Item -Path $installDir -Recurse -Force
        Write-Output "  Removed $installDir"
    }

    # --- Remove Start Menu shortcut ---
    $shortcutPath = Join-Path $startMenuDir "$appName.lnk"
    if (Test-Path $shortcutPath) {
        Remove-Item -Path $shortcutPath -Force
        Write-Output "  Removed Start Menu shortcut"
    }
}

# --- Tidy: remove install root if empty ---
if ((Test-Path $installRoot) -and -not (Get-ChildItem -Path $installRoot -Force)) {
    Remove-Item -Path $installRoot -Force
}

Write-Output ""
Write-Output "Done. Open a new terminal so PATH changes take effect."
