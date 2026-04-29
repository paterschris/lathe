[CmdletBinding()]
Param(
    [Parameter()][Alias('a')][string]$Architecture
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Set-Location (Join-Path $PSScriptRoot '..')

# --- Architecture and target ---
$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    'X64'   { 'x86_64' }
    'Arm64' { 'aarch64' }
    default { throw "Unsupported architecture" }
}
if (-not $Architecture) { $Architecture = $OSArchitecture }
$Target = "$Architecture-pc-windows-msvc"

# --- Channel + naming (mirrors package-fork-linux) ---
$channel = (Get-Content 'crates/zed/RELEASE_CHANNEL').Trim()
$suffix = if ($channel -ne 'stable') { "-$channel" } else { '' }

$appName = switch ($channel) {
    'stable'  { 'Lathe' }
    'preview' { 'Lathe Preview' }
    'beta'    { 'Lathe Beta' }
    'nightly' { 'Lathe Nightly' }
    default   { 'Lathe Dev' }
}
$iconChannel = switch ($channel) {
    'stable'  { '' }
    default   { "-$channel" }
}

# --- Version ---
$cargoToml = Get-Content 'crates/zed/Cargo.toml'
$versionLine = $cargoToml | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1
if ($versionLine -notmatch '"([^"]+)"') {
    Write-Error "Could not parse version from crates/zed/Cargo.toml"
    exit 1
}
$version = $matches[1]

Write-Output "=== Packaging $appName (Windows) ==="
Write-Output "  Version: $version"
Write-Output "  Arch:    $Architecture"
Write-Output "  Channel: $channel"

# --- Verify binaries ---
$cargoOut = "target/$Target/release"
$zedExe = "$cargoOut/zed.exe"
$cliExe = "$cargoOut/cli.exe"

if (-not (Test-Path $zedExe)) {
    Write-Error "zed.exe not found at $zedExe. Run 'script/build-fork-windows.ps1' first."
    exit 1
}
if (-not (Test-Path $cliExe)) {
    Write-Error "cli.exe not found at $cliExe. Run 'script/build-fork-windows.ps1' first."
    exit 1
}

# --- Stage layout (mirrors Linux app/bin/libexec/lib/share split) ---
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
try {
    $appDir = Join-Path $tempDir "lathe$suffix"
    New-Item -ItemType Directory -Path "$appDir/bin"     -Force | Out-Null
    New-Item -ItemType Directory -Path "$appDir/libexec" -Force | Out-Null
    New-Item -ItemType Directory -Path "$appDir/lib"     -Force | Out-Null
    New-Item -ItemType Directory -Path "$appDir/share"   -Force | Out-Null

    Copy-Item $zedExe "$appDir/libexec/lathe-editor.exe" -Force
    Copy-Item $cliExe "$appDir/bin/lathe.exe"            -Force

    # --- Bundle ConPTY (terminal runtime) ---
    Write-Output "--- Downloading ConPTY ---"
    $conptyUrl = 'https://github.com/microsoft/terminal/releases/download/v1.23.13503.0/Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg'
    $conptyNupkg = Join-Path $tempDir 'conpty.nupkg'
    $conptyExtract = Join-Path $tempDir 'conpty'
    Invoke-WebRequest -Uri $conptyUrl -OutFile $conptyNupkg
    Expand-Archive -Path $conptyNupkg -DestinationPath $conptyExtract -Force

    if ($Architecture -eq 'aarch64') {
        Copy-Item "$conptyExtract/runtimes/win-arm64/native/conpty.dll"     "$appDir/lib/conpty.dll"     -Force
        Copy-Item "$conptyExtract/build/native/runtimes/arm64/OpenConsole.exe" "$appDir/lib/OpenConsole.exe" -Force
    } else {
        Copy-Item "$conptyExtract/runtimes/win-x64/native/conpty.dll"     "$appDir/lib/conpty.dll"     -Force
        Copy-Item "$conptyExtract/build/native/runtimes/x64/OpenConsole.exe" "$appDir/lib/OpenConsole.exe" -Force
    }

    # --- Bundle AMD AGS SDK (x86_64 only, optional GPU detection) ---
    if ($Architecture -eq 'x86_64') {
        Write-Output "--- Downloading AMD AGS SDK ---"
        $agsUrl = 'https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0'
        $agsZip = Join-Path $tempDir 'ags.zip'
        $agsExtract = Join-Path $tempDir 'ags'
        Invoke-WebRequest -Uri $agsUrl -OutFile $agsZip
        Expand-Archive -Path $agsZip -DestinationPath $agsExtract -Force
        Copy-Item "$agsExtract/AGS_SDK-6.3.0/ags_lib/lib/amd_ags_x64.dll" "$appDir/lib/amd_ags_x64.dll" -Force
    }

    # --- Icon ---
    Write-Output "--- Copying icon ---"
    $iconSrc = "crates/zed/resources/windows/app-icon$iconChannel.ico"
    if (-not (Test-Path $iconSrc)) {
        $iconSrc = 'crates/zed/resources/windows/app-icon.ico'
    }
    Copy-Item $iconSrc "$appDir/share/lathe.ico" -Force

    # --- Licenses (if generated) ---
    if (Test-Path 'assets/licenses.md') {
        Copy-Item 'assets/licenses.md' "$appDir/licenses.md" -Force
    }

    # --- Archive ---
    $outDir = 'target/release'
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null
    $archiveName = "Lathe-$version-$Architecture-windows.zip"
    $archivePath = Join-Path $outDir $archiveName

    Write-Output "--- Creating archive ---"
    if (Test-Path $archivePath) { Remove-Item $archivePath -Force }
    Compress-Archive -Path $appDir -DestinationPath $archivePath

    Write-Output ""
    Write-Output "=== Package created ==="
    Write-Output "  $archivePath"
    Write-Output ""
    Write-Output "To install locally: script/install-fork-windows.ps1"
}
finally {
    Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
