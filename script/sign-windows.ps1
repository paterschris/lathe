[CmdletBinding()]
Param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$Files
)

# Authenticode-signs Windows artifacts with Azure Artifact Signing (the
# service formerly called Trusted Signing). Called by
# script/package-fork-windows.ps1 for the staged binaries, and by ISCC via
# the /sDefaultsign sign-tool hook for the installer and uninstaller.
#
# Configuration comes from the environment so an unconfigured checkout still
# packages successfully:
#   LATHE_SIGN_ENDPOINT  - regional endpoint, e.g. https://eus.codesigning.azure.net/
#   LATHE_SIGN_ACCOUNT   - Artifact Signing account name
#   LATHE_SIGN_PROFILE   - certificate profile name
#   LATHE_REQUIRE_SIGNING - when truthy, missing config is a hard error rather
#                           than a skip (set for release builds, matching the
#                           macOS lane in .github/workflows/release_fork.yml)
#
# Azure credentials are resolved by DefaultAzureCredential inside the
# TrustedSigning module, so both auth modes work without changes here:
# an `azure/login` OIDC session (via the Azure CLI credential), or
# AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET (environment
# credential). The signing identity needs the "Artifact Signing Certificate
# Profile Signer" role on the account.

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$endpoint = $env:LATHE_SIGN_ENDPOINT
$account = $env:LATHE_SIGN_ACCOUNT
$profileName = $env:LATHE_SIGN_PROFILE

$missing = @()
if ([string]::IsNullOrWhiteSpace($endpoint)) { $missing += 'LATHE_SIGN_ENDPOINT' }
if ([string]::IsNullOrWhiteSpace($account)) { $missing += 'LATHE_SIGN_ACCOUNT' }
if ([string]::IsNullOrWhiteSpace($profileName)) { $missing += 'LATHE_SIGN_PROFILE' }

if ($missing.Count -gt 0) {
    $detail = "Windows code signing is not configured (missing: $($missing -join ', '))."
    if ($env:LATHE_REQUIRE_SIGNING -and $env:LATHE_REQUIRE_SIGNING -ne '0') {
        throw "$detail LATHE_REQUIRE_SIGNING is set, refusing to produce unsigned artifacts."
    }
    Write-Warning "$detail Artifacts will be unsigned and SmartScreen will warn users."
    exit 0
}

# The runner image doesn't ship the module. Note that the service kept the
# module name "TrustedSigning" through the rename to Azure Artifact Signing;
# there is no Invoke-ArtifactSigning cmdlet.
if (-not (Get-Module -ListAvailable -Name TrustedSigning)) {
    Write-Output "--- Installing TrustedSigning module ---"
    # Install-Module silently bootstraps NuGet interactively if the provider
    # is missing, which hangs a CI job rather than failing it.
    if (-not (Get-PackageProvider -Name NuGet -ErrorAction SilentlyContinue)) {
        Install-PackageProvider -Name NuGet -Force -Scope CurrentUser | Out-Null
    }
    Install-Module -Name TrustedSigning -Force -Scope CurrentUser -AllowClobber -Repository PSGallery
}

# Certificates issued by the service are valid for three days, so an
# untimestamped signature would expire almost immediately.
$timestampServer = if ([string]::IsNullOrWhiteSpace($env:LATHE_SIGN_TIMESTAMP_SERVER)) {
    'http://timestamp.acs.microsoft.com'
} else {
    $env:LATHE_SIGN_TIMESTAMP_SERVER
}

$resolved = @()
foreach ($file in $Files) {
    # Both callers pass absolute paths (ISCC one per invocation, the packaging
    # script from its temp stage dir); resolving also fails loudly on a typo
    # rather than letting the service reject the path later.
    $resolved += (Resolve-Path -LiteralPath $file).Path
}

Write-Output "--- Signing $($resolved.Count) file(s) with Artifact Signing ---"
foreach ($file in $resolved) {
    Write-Output "    $file"
}

Invoke-TrustedSigning `
    -Endpoint $endpoint `
    -CodeSigningAccountName $account `
    -CertificateProfileName $profileName `
    -Files ($resolved -join ',') `
    -FileDigest 'SHA256' `
    -TimestampRfc3161 $timestampServer `
    -TimestampDigest 'SHA256'

foreach ($file in $resolved) {
    $signature = Get-AuthenticodeSignature -LiteralPath $file
    if ($signature.Status -ne 'Valid') {
        throw "Signing reported success but $file has signature status '$($signature.Status)'"
    }
}

Write-Output "--- Signed successfully ---"
