[CmdletBinding()]
Param(
    [Parameter()][Alias('a')][string]$Architecture
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

Set-Location (Join-Path $PSScriptRoot '..')

Write-Output "=== Building Lathe (Windows) ==="

# --- Architecture resolution ---
$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    'X64'   { 'x86_64' }
    'Arm64' { 'aarch64' }
    default { throw "Unsupported architecture" }
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

$vsRoots = @(
    'C:\Program Files\Microsoft Visual Studio\2022\Community',
    'C:\Program Files\Microsoft Visual Studio\2022\Professional',
    'C:\Program Files\Microsoft Visual Studio\2022\Enterprise',
    'C:\Program Files\Microsoft Visual Studio\2022\BuildTools'
)
$vsDevShell = $null
foreach ($root in $vsRoots) {
    $candidate = Join-Path $root 'Common7\Tools\Launch-VsDevShell.ps1'
    if (Test-Path $candidate) { $vsDevShell = $candidate; break }
}

if (-not $vsDevShell) {
    Write-Error "Visual Studio 2022 not found. Install VS 2022 with the C++ workload, or VS Build Tools."
    exit 1
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
