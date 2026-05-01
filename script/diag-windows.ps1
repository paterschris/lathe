[CmdletBinding()]
Param(
    [Parameter()][string]$InstallRoot = "$env:LOCALAPPDATA\Programs\Lathe\lathe-beta",
    [Parameter()][int]$EditorTimeoutSeconds = 15
)

# Best-effort diagnostic for "lathe-editor.exe launches but no window appears"
# style failures on a Windows install. Prints install layout, signature
# state, Mark-of-the-Web, Defender threats/exclusions, recent Application
# event log entries mentioning lathe/zed, the latest Zed log tail, and the
# editor's stdout/stderr when launched with --foreground.
#
# Usage:
#   .\script\diag-windows.ps1
#   .\script\diag-windows.ps1 -InstallRoot "C:\custom\path\lathe-beta"
#
# Safe to run without admin; some Defender cmdlets degrade gracefully if
# they're unavailable. Output is plain text — paste it back as-is.

$ErrorActionPreference = 'Continue'

function Section([string]$title) {
    Write-Output ""
    Write-Output "=== $title ==="
}

Write-Output "Lathe Windows install diagnostic"
Write-Output "  InstallRoot: $InstallRoot"
Write-Output "  Host:        $env:COMPUTERNAME ($([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture))"
Write-Output "  PowerShell:  $($PSVersionTable.PSVersion)"
Write-Output "  Date:        $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz')"

# ---------------------------------------------------------------------------

Section "Install layout"
if (-not (Test-Path $InstallRoot)) {
    Write-Output "MISSING: $InstallRoot does not exist."
    exit 1
}
Get-ChildItem -Path $InstallRoot -Recurse -File |
    Sort-Object FullName |
    ForEach-Object {
        "{0,12:N0}  {1}" -f $_.Length, $_.FullName.Substring($InstallRoot.Length).TrimStart('\')
    }

# ---------------------------------------------------------------------------

Section "Running Lathe/Zed processes"
$procs = Get-Process -ErrorAction SilentlyContinue |
    Where-Object { $_.ProcessName -match '^(lathe|zed)' }
if ($procs) {
    $procs | Select-Object Id, ProcessName, Path, StartTime |
        Format-Table -AutoSize | Out-String | Write-Output
} else {
    Write-Output "(none)"
}

# ---------------------------------------------------------------------------

Section "Authenticode signature"
foreach ($rel in @('bin\lathe.exe', 'libexec\lathe-editor.exe')) {
    $exe = Join-Path $InstallRoot $rel
    if (-not (Test-Path $exe)) { continue }
    $sig = Get-AuthenticodeSignature $exe
    Write-Output "  $rel"
    Write-Output "    Status: $($sig.Status) ($($sig.StatusMessage))"
    if ($sig.SignerCertificate) {
        Write-Output "    Signer: $($sig.SignerCertificate.Subject)"
    } else {
        Write-Output "    Signer: (unsigned)"
    }
}

# ---------------------------------------------------------------------------

Section "Mark-of-the-Web (Zone.Identifier) on editor exe"
$editor = Join-Path $InstallRoot 'libexec\lathe-editor.exe'
if (Test-Path $editor) {
    try {
        $zone = Get-Content -Path $editor -Stream Zone.Identifier -ErrorAction Stop
        Write-Output "ZoneId present (downloaded-from-internet flag still attached):"
        $zone | ForEach-Object { Write-Output "  $_" }
    } catch {
        Write-Output "(no Zone.Identifier — file is not flagged as downloaded)"
    }
} else {
    Write-Output "MISSING: $editor"
}

# ---------------------------------------------------------------------------

Section "Defender threats matching lathe/zed"
try {
    $threats = Get-MpThreat -ErrorAction Stop | Where-Object { $_.Resources -match 'lathe|zed' }
    if ($threats) {
        $threats | Format-List | Out-String | Write-Output
    } else {
        Write-Output "(none)"
    }
} catch {
    Write-Output "Get-MpThreat unavailable: $($_.Exception.Message)"
}

Section "Defender exclusions"
try {
    $prefs = Get-MpPreference -ErrorAction Stop
    if ($prefs.ExclusionPath) {
        $prefs.ExclusionPath | ForEach-Object { Write-Output "  $_" }
    } else {
        Write-Output "(none)"
    }
} catch {
    Write-Output "Get-MpPreference unavailable: $($_.Exception.Message)"
}

# ---------------------------------------------------------------------------

Section "Recent Application event log (last 2h, lathe/zed)"
try {
    $events = Get-WinEvent -FilterHashtable @{
        LogName   = 'Application'
        StartTime = (Get-Date).AddHours(-2)
    } -ErrorAction Stop |
        Where-Object { $_.Message -match 'lathe|zed' } |
        Select-Object -First 15
    if ($events) {
        foreach ($e in $events) {
            $first = ($e.Message -split "`n" | Select-Object -First 1).Trim()
            Write-Output "  [$($e.TimeCreated)] $($e.LevelDisplayName)/$($e.ProviderName): $first"
        }
    } else {
        Write-Output "(no matching events)"
    }
} catch {
    Write-Output "Get-WinEvent failed: $($_.Exception.Message)"
}

# ---------------------------------------------------------------------------

Section "Lathe/Zed log dirs"
$logCandidates = @(
    "$env:LOCALAPPDATA\Zed\logs",
    "$env:APPDATA\Zed\logs",
    "$env:USERPROFILE\.config\zed\logs"
)
$primaryLog = $null
foreach ($dir in $logCandidates) {
    if (Test-Path $dir) {
        Write-Output "  $dir"
        Get-ChildItem $dir -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending |
            Select-Object -First 5 |
            ForEach-Object {
                Write-Output ("    {0:yyyy-MM-dd HH:mm:ss}  {1,10:N0}  {2}" -f $_.LastWriteTime, $_.Length, $_.Name)
            }
        if (-not $primaryLog) {
            $primaryLog = Join-Path $dir 'Zed.log'
            if (-not (Test-Path $primaryLog)) { $primaryLog = $null }
        }
    }
}
if (-not $primaryLog) {
    Write-Output "(no log dirs found)"
}

if ($primaryLog) {
    Section "Tail of $primaryLog (last 80 lines)"
    Get-Content $primaryLog -Tail 80 | ForEach-Object { Write-Output "  $_" }
}

# ---------------------------------------------------------------------------

Section "Editor in foreground (timeout: ${EditorTimeoutSeconds}s)"
if (-not (Test-Path $editor)) {
    Write-Output "MISSING: $editor"
    Write-Output "Done."
    exit 1
}

$stdoutFile = [System.IO.Path]::GetTempFileName()
$stderrFile = [System.IO.Path]::GetTempFileName()
Write-Output "Spawning: $editor --foreground"
Write-Output "(window may or may not appear; capturing stdout/stderr regardless)"
try {
    $proc = Start-Process -FilePath $editor -ArgumentList '--foreground' `
        -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile `
        -PassThru -WindowStyle Hidden
    if (-not $proc.WaitForExit($EditorTimeoutSeconds * 1000)) {
        Write-Output "Process still running after ${EditorTimeoutSeconds}s (PID $($proc.Id)); killing."
        $proc.Kill()
        $proc.WaitForExit()
        Write-Output "Exit code (after kill): $($proc.ExitCode)"
    } else {
        Write-Output "Exit code: $($proc.ExitCode)"
    }
    Write-Output "--- stdout ---"
    if ((Get-Item $stdoutFile).Length -gt 0) {
        Get-Content $stdoutFile | ForEach-Object { Write-Output "  $_" }
    } else {
        Write-Output "  (empty)"
    }
    Write-Output "--- stderr ---"
    if ((Get-Item $stderrFile).Length -gt 0) {
        Get-Content $stderrFile | ForEach-Object { Write-Output "  $_" }
    } else {
        Write-Output "  (empty)"
    }
} finally {
    Remove-Item $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue
}

Write-Output ""
Write-Output "Done."
