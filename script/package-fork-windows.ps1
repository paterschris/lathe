[CmdletBinding()]
Param(
    [Parameter()][Alias('a')][string]$Architecture
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Set-Location (Join-Path $PSScriptRoot '..')

# --- Architecture and target ---
# See build-fork-windows.ps1: RuntimeInformation::OSArchitecture lies
# about the OS arch under Windows PowerShell 5.1's .NET Framework.
$osArchRaw = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$OSArchitecture = switch ($osArchRaw) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "Unsupported architecture: $osArchRaw" }
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

    # conpty.dll is loaded with LoadLibraryW("conpty.dll"), which searches the
    # editor exe's own directory, and conpty locates OpenConsole.exe in an
    # arch-named subdirectory next to itself (upstream ships {app}\conpty.dll
    # and {app}\x64\OpenConsole.exe next to Zed.exe). Stage both beside
    # lathe-editor.exe, NOT in lib\, or the terminal runs degraded.
    if ($Architecture -eq 'aarch64') {
        New-Item -ItemType Directory -Path "$appDir/libexec/arm64" -Force | Out-Null
        Copy-Item "$conptyExtract/runtimes/win-arm64/native/conpty.dll"     "$appDir/libexec/conpty.dll"     -Force
        Copy-Item "$conptyExtract/build/native/runtimes/arm64/OpenConsole.exe" "$appDir/libexec/arm64/OpenConsole.exe" -Force
    } else {
        New-Item -ItemType Directory -Path "$appDir/libexec/x64" -Force | Out-Null
        Copy-Item "$conptyExtract/runtimes/win-x64/native/conpty.dll"     "$appDir/libexec/conpty.dll"     -Force
        Copy-Item "$conptyExtract/build/native/runtimes/x64/OpenConsole.exe" "$appDir/libexec/x64/OpenConsole.exe" -Force
    }

    # --- Bundle AMD AGS SDK (x86_64 only, optional GPU detection) ---
    if ($Architecture -eq 'x86_64') {
        Write-Output "--- Downloading AMD AGS SDK ---"
        $agsUrl = 'https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0'
        $agsZip = Join-Path $tempDir 'ags.zip'
        $agsExtract = Join-Path $tempDir 'ags'
        Invoke-WebRequest -Uri $agsUrl -OutFile $agsZip
        Expand-Archive -Path $agsZip -DestinationPath $agsExtract -Force
        # Loaded by name at runtime, so it must sit in the editor exe's directory.
        Copy-Item "$agsExtract/AGS_SDK-6.3.0/ags_lib/lib/amd_ags_x64.dll" "$appDir/libexec/amd_ags_x64.dll" -Force
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

    # --- Code signing ---
    # Sign before archiving so the zip and the installer both carry signed
    # binaries. Only the two Lathe executables are signed; conpty.dll,
    # OpenConsole.exe and amd_ags_x64.dll already ship signed by Microsoft
    # and AMD, and re-signing them would strip those signatures.
    Write-Output ""
    Write-Output "--- Code signing ---"
    & "$PSScriptRoot/sign-windows.ps1" "$appDir/libexec/lathe-editor.exe" "$appDir/bin/lathe.exe"

    # --- Archive ---
    $outDir = 'target/release'
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null
    $archiveName = "Lathe-$version-$Architecture-windows.zip"
    $archivePath = Join-Path $outDir $archiveName

    Write-Output "--- Creating archive ---"
    if (Test-Path $archivePath) { Remove-Item $archivePath -Force }
    Compress-Archive -Path $appDir -DestinationPath $archivePath

    Write-Output ""
    Write-Output "=== Zip created ==="
    Write-Output "  $archivePath"

    # --- Compile Inno Setup installer (.exe) ---
    # Skipped if ISCC.exe isn't on the machine (e.g. a dev workstation
    # without Inno Setup installed). On GitHub-hosted windows-2022 it's
    # pre-installed. Locally: `choco install innosetup` or download from
    # https://jrsoftware.org/isdl.php.
    Write-Output ""
    Write-Output "--- Compiling Inno Setup installer ---"
    $iscc = $null
    foreach ($candidate in @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )) {
        if (Test-Path $candidate) { $iscc = $candidate; break }
    }
    if (-not $iscc) {
        $cmd = Get-Command ISCC -ErrorAction SilentlyContinue
        if ($cmd) { $iscc = $cmd.Source }
    }

    if (-not $iscc) {
        Write-Warning "ISCC.exe not found; skipping installer build."
        Write-Warning "Install Inno Setup from https://jrsoftware.org/isdl.php or 'choco install innosetup'."
    } else {
        Write-Output "  Compiler: $iscc"
        $outAbs = (Resolve-Path $outDir).Path
        $issPath = (Resolve-Path 'crates/zed/resources/windows/lathe.iss').Path
        $isccArgs = @(
            '/Qp',
            "/DLatheChannel=$channel",
            "/DLatheVersion=$version",
            "/DLatheArch=$Architecture",
            "/DStageDir=$appDir",
            "/DOutputDir=$outAbs"
        )

        # Inno signs the installer (and, via SignedUninstaller, the extracted
        # uninstaller) by shelling out to this command with $f replaced by the
        # file to sign. Only register it when signing is configured, so an
        # unconfigured build doesn't spawn a PowerShell per file just to warn.
        if ($env:LATHE_SIGN_ENDPOINT -and $env:LATHE_SIGN_ACCOUNT -and $env:LATHE_SIGN_PROFILE) {
            $signScript = (Resolve-Path "$PSScriptRoot/sign-windows.ps1").Path
            # Prefer pwsh: this script already ran the sign helper once under
            # whichever host invoked it, and PowerShell 5.1 resolves
            # CurrentUser modules from a different directory, so mixing hosts
            # would install TrustedSigning a second time mid-compile.
            $signHost = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh' } else { 'powershell.exe' }
            $isccArgs += '/DLatheSignTool=1'
            $isccArgs += "/sDefaultsign=$signHost -NoProfile -ExecutionPolicy Bypass -File `"$signScript`" `$f"
        }

        & $iscc @isccArgs $issPath
        if ($LASTEXITCODE -ne 0) {
            throw "ISCC.exe failed with exit code $LASTEXITCODE"
        }
        $setupExe = Join-Path $outDir "Lathe-$version-$Architecture-windows-setup.exe"
        Write-Output ""
        Write-Output "=== Installer created ==="
        Write-Output "  $setupExe"
    }

    Write-Output ""
    Write-Output "To install locally from the zip: script/install-fork-windows.ps1"
}
finally {
    Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
