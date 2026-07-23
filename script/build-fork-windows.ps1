[CmdletBinding()]
Param(
    [Parameter()][Alias('a')][string]$Architecture
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Set-Location (Join-Path $PSScriptRoot '..')

Write-Output "=== Building Lathe (Windows) ==="

# --- Architecture resolution ---
# PROCESSOR_ARCHITEW6432 is set when running in a 32-bit process on a
# 64-bit OS. Don't use RuntimeInformation::OSArchitecture here: on
# Windows PowerShell 5.1 (.NET Framework) it reports the PROCESS
# architecture, so a 32-bit shell on x64 yields X86 and a bogus
# "Unsupported architecture" abort.
$osArchRaw = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$OSArchitecture = switch ($osArchRaw) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "Unsupported architecture: $osArchRaw" }
}

if (-not $Architecture) {
    $Architecture = $OSArchitecture
}

$Target = "$Architecture-pc-windows-msvc"
Write-Output "Target: $Target"

# --- Rust toolchain ---
function Install-Rust {
    Write-Output "Installing Rust via rustup..."
    $rustupInit = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $rustupInit
    & $rustupInit -y --default-toolchain stable
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path $cargoBin) {
        $env:Path = "$cargoBin;$env:Path"
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path $cargoBin) {
        $env:Path = "$cargoBin;$env:Path"
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Install-Rust
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Failed to install Rust. Install manually from https://rustup.rs"
    exit 1
}

rustup target add $Target

# --- Visual Studio dev shell (provides link.exe, libs) ---
function Get-VSArch {
    param([string]$Arch)
    switch ($Arch) {
        'x86_64'  { 'amd64' }
        'aarch64' { 'arm64' }
    }
}

function Find-VsDevShell {
    # vswhere ships with any VS installer and knows about custom install
    # locations the hardcoded roots below would miss.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1
        if ($installPath) {
            $candidate = Join-Path $installPath 'Common7\Tools\Launch-VsDevShell.ps1'
            if (Test-Path $candidate) { return $candidate }
        }
    }
    $vsRoots = @(
        'C:\Program Files\Microsoft Visual Studio\2022\Community',
        'C:\Program Files\Microsoft Visual Studio\2022\Professional',
        'C:\Program Files\Microsoft Visual Studio\2022\Enterprise',
        'C:\Program Files\Microsoft Visual Studio\2022\BuildTools'
    )
    foreach ($root in $vsRoots) {
        $candidate = Join-Path $root 'Common7\Tools\Launch-VsDevShell.ps1'
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

$vsDevShell = Find-VsDevShell

if (-not $vsDevShell) {
    # Mirror the rustup bootstrap above: fetch and install VS Build Tools
    # with the C++ workload. The installer needs elevation, so expect a
    # UAC prompt; --passive shows progress for what is a multi-GB,
    # 10-30 minute install.
    Write-Output "Visual Studio 2022 C++ tools not found -- downloading Build Tools installer..."
    $bootstrapper = Join-Path $env:TEMP 'vs_BuildTools.exe'
    Invoke-WebRequest -Uri 'https://aka.ms/vs/17/release/vs_BuildTools.exe' -OutFile $bootstrapper
    $installerArgs = @(
        '--passive', '--wait', '--norestart',
        '--add', 'Microsoft.VisualStudio.Workload.VCTools',
        # msvc_spectre_libs' build script hard-requires the
        # Spectre-mitigated runtime libs (see docs/src/development/windows.md).
        '--add', 'Microsoft.VisualStudio.Component.VC.Runtimes.x86.x64.Spectre',
        '--includeRecommended'
    )
    if ($Architecture -eq 'aarch64' -or $OSArchitecture -eq 'aarch64') {
        $installerArgs += @(
            '--add', 'Microsoft.VisualStudio.Component.VC.Tools.ARM64',
            '--add', 'Microsoft.VisualStudio.Component.VC.Runtimes.ARM64.Spectre'
        )
    }
    Write-Output "Installing Build Tools (this takes a while)..."
    $installerProcess = Start-Process -FilePath $bootstrapper -ArgumentList $installerArgs -Verb RunAs -Wait -PassThru
    # 3010 = success, reboot required; the linker works without one.
    if ($installerProcess.ExitCode -ne 0 -and $installerProcess.ExitCode -ne 3010) {
        Write-Error "Build Tools installer exited with $($installerProcess.ExitCode). Install manually from https://visualstudio.microsoft.com/downloads/ with the 'Desktop development with C++' workload."
        exit 1
    }
    $vsDevShell = Find-VsDevShell
}

if (-not $vsDevShell) {
    Write-Error "Visual Studio 2022 C++ tools still not found after install. Install manually with the 'Desktop development with C++' workload."
    exit 1
}

# --- Spectre-mitigated libs (msvc_spectre_libs hard-requires them) ---
# A pre-existing VS/Build Tools install may lack them (the plain VCTools
# workload doesn't include them); detect and add the component via the
# installed VS Installer engine, which handles every edition. UAC prompt
# expected.
$vsRoot = Split-Path (Split-Path (Split-Path $vsDevShell -Parent) -Parent) -Parent
$spectreLibDir = if ($Architecture -eq 'aarch64') { 'arm64' } else { 'x64' }
$spectreComponent = if ($Architecture -eq 'aarch64') {
    'Microsoft.VisualStudio.Component.VC.Runtimes.ARM64.Spectre'
} else {
    'Microsoft.VisualStudio.Component.VC.Runtimes.x86.x64.Spectre'
}
function Test-SpectreLibs {
    [bool](Get-ChildItem -Path (Join-Path $vsRoot "VC\Tools\MSVC\*\lib\spectre\$spectreLibDir\*.lib") -ErrorAction SilentlyContinue | Select-Object -First 1)
}
if (-not (Test-SpectreLibs)) {
    Write-Output "Spectre-mitigated libs not found in $vsRoot -- adding component (expect a UAC prompt)..."
    $vsInstaller = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\setup.exe'
    if (-not (Test-Path $vsInstaller)) {
        Write-Error "Visual Studio Installer not found at $vsInstaller. Add '$spectreComponent' manually via the Visual Studio Installer."
        exit 1
    }
    $modifyArgs = @('modify', '--installPath', $vsRoot, '--passive', '--norestart', '--add', $spectreComponent)
    $modifyProcess = Start-Process -FilePath $vsInstaller -ArgumentList $modifyArgs -Verb RunAs -Wait -PassThru
    if ($modifyProcess.ExitCode -ne 0 -and $modifyProcess.ExitCode -ne 3010) {
        Write-Error ("Visual Studio Installer exited with $($modifyProcess.ExitCode) while adding $spectreComponent. " +
            "A pending reboot from a previous install commonly causes this: reboot and rerun this script. " +
            "Otherwise add the component via the Visual Studio Installer GUI (Modify > Individual components > Spectre-mitigated libs).")
        exit 1
    }
    if (-not (Test-SpectreLibs)) {
        Write-Error "Spectre-mitigated libs still missing after the modify. Add '$spectreComponent' manually via the Visual Studio Installer."
        exit 1
    }
    Write-Output "Spectre-mitigated libs installed."
}

Push-Location
& $vsDevShell -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture) | Out-Null
Pop-Location

# --- Channel and build env ---
$channel = (Get-Content 'crates/zed/RELEASE_CHANNEL').Trim()
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
$env:ZED_BUNDLE = 'true'
# Mirrors upstream bundling: compiled into GPUI so the binary knows its
# release version (updater comparisons, version reporting).
$versionLine = Select-String -Path 'crates/zed/Cargo.toml' -Pattern '^version = "(.*)"' | Select-Object -First 1
$env:RELEASE_VERSION = $versionLine.Matches[0].Groups[1].Value
# Embed the commit like upstream so the startup banner doesn't say
# "sha unknown". CI sets this via github.sha; locally git works. try/catch
# because PSNativeCommandUseErrorActionPreference turns git failures into
# terminating errors.
if (-not $env:ZED_COMMIT_SHA) {
    try {
        $commitSha = (& git rev-parse HEAD 2>$null)
        if ($commitSha) { $env:ZED_COMMIT_SHA = "$commitSha".Trim() }
    } catch {
        Write-Output "Note: could not determine commit sha; banner will say unknown."
    }
}

Write-Output "Channel: $channel"
Write-Output "Building release binaries..."

cargo build --release --target $Target --package zed --package cli

$outDir = "target/$Target/release"
Write-Output ""
Write-Output "=== Build complete ==="
Write-Output "  GUI:    $outDir/zed.exe"
Write-Output "  CLI:    $outDir/cli.exe"
Write-Output ""
Write-Output "To package: script/package-fork-windows.ps1"
