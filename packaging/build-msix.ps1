<#
.SYNOPSIS
    Builds Mandala Explorer as an MSIX package.

.DESCRIPTION
    Builds the release binary, lays out a package directory, and packs it.

    Signing is optional and only for testing locally: the Store signs the
    package itself on submission, and expects an unsigned one. A locally signed
    package can be installed to check that the manifest, the icons and the
    full-trust launch actually work before uploading anything.

    The certificate's subject has to match the manifest's Publisher exactly,
    or Windows refuses the package with an error that does not say so. This
    script reads the Publisher out of the manifest rather than repeating it.

.PARAMETER Version
    Four-part package version. The Store requires the revision (fourth) part
    to be 0, and every submission to be higher than the last.

.PARAMETER Sign
    Sign with a self-signed test certificate, creating one if needed.

.EXAMPLE
    .\packaging\build-msix.ps1 -Version 0.1.0.0 -Sign
#>
[CmdletBinding()]
param(
    [ValidatePattern('^\d+\.\d+\.\d+\.0$')]
    [string]$Version = '0.1.0.0',
    [switch]$Sign,
    [string]$OutDir = 'target/msix'
)

# Stop on cmdlet failures, but not on native tools: cargo and makeappx write
# progress to stderr, and Windows PowerShell turns a redirected native stderr
# into a terminating error regardless of the exit code. Their output is left
# alone and their exit codes are checked by hand.
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    # --- tools -------------------------------------------------------------
    $kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
    $sdk = Get-ChildItem $kits -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'x64\makeappx.exe') } |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $sdk) { throw "No Windows SDK with makeappx.exe found under $kits" }
    $makeappx = Join-Path $sdk.FullName 'x64\makeappx.exe'
    $signtool = Join-Path $sdk.FullName 'x64\signtool.exe'
    Write-Host "SDK: $($sdk.Name)"

    # --- inputs ------------------------------------------------------------
    Write-Host 'Generating icon assets'
    & python packaging/make_assets.py packaging/Assets
    if ($LASTEXITCODE -ne 0) { throw 'asset generation failed' }

    Write-Host 'Building release binary'
    & cargo build --release -p mandala-app
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

    # --- layout ------------------------------------------------------------
    $layout = Join-Path $OutDir 'layout'
    if (Test-Path $layout) { Remove-Item $layout -Recurse -Force }
    New-Item -ItemType Directory -Force -Path (Join-Path $layout 'Assets') | Out-Null

    Copy-Item 'target/release/mandala.exe' $layout
    Copy-Item 'packaging/Assets/*.png' (Join-Path $layout 'Assets')

    # The version lives in one place at rest and is stamped in on the way out,
    # so a build cannot ship a number that disagrees with what was asked for.
    $manifest = Get-Content 'packaging/AppxManifest.xml' -Raw
    $manifest = $manifest -replace '(<Identity[^>]*?Version=")[^"]*(")', "`${1}$Version`${2}"
    [System.IO.File]::WriteAllText(
        (Join-Path $layout 'AppxManifest.xml'), $manifest, (New-Object System.Text.UTF8Encoding($false)))

    if ($manifest -notmatch 'Publisher="([^"]+)"') { throw 'no Publisher in the manifest' }
    $publisher = $Matches[1]
    Write-Host "Publisher: $publisher"
    Write-Host "Version:   $Version"

    # --- pack --------------------------------------------------------------
    $msix = Join-Path $OutDir 'MandalaExplorer.msix'
    if (Test-Path $msix) { Remove-Item $msix -Force }
    & $makeappx pack /d $layout /p $msix /o
    if ($LASTEXITCODE -ne 0) { throw 'makeappx failed' }

    # --- optional test signature -------------------------------------------
    if ($Sign) {
        $cert = Get-ChildItem Cert:\CurrentUser\My |
            Where-Object { $_.Subject -eq $publisher } | Select-Object -First 1
        if (-not $cert) {
            Write-Host "Creating a test certificate for $publisher"
            $cert = New-SelfSignedCertificate -Type Custom -Subject $publisher `
                -KeyUsage DigitalSignature -FriendlyName 'Mandala Explorer test signing' `
                -CertStoreLocation 'Cert:\CurrentUser\My' `
                -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
            Write-Host "Thumbprint: $($cert.Thumbprint)"
            Write-Host 'To trust it (needs an elevated shell):'
            Write-Host "  Export-Certificate -Cert Cert:\CurrentUser\My\$($cert.Thumbprint) -FilePath test.cer"
            Write-Host '  Import-Certificate -FilePath test.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople'
        }
        & $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $msix
        if ($LASTEXITCODE -ne 0) { throw 'signtool failed' }
    }

    $size = (Get-Item $msix).Length / 1MB
    Write-Host ''
    Write-Host ("Built {0} ({1:N1} MB)" -f $msix, $size)
    if (-not $Sign) {
        Write-Host 'Unsigned, which is what the Store wants. Pass -Sign to install it locally.'
    }
}
finally {
    Pop-Location
}
