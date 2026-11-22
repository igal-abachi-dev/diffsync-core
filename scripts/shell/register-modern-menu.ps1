<#
.SYNOPSIS
  Puts the DiffSync entries into the Windows 11 top-level context menu through a sparse package.

.DESCRIPTION
  Windows 11 shows a shell extension in the new menu only when it has a package identity. This
  script builds a *sparse* package from shell\AppxManifest.xml (the DLL stays where it is), signs it
  with a self-signed certificate, and registers it with the DLL's folder as the external location.

  STATUS: written from Microsoft's documented procedure; it has not been run in this repository's
  test runs, because it needs Windows 11, the Windows SDK (makeappx.exe, signtool.exe) and
  administrator rights once (to trust the certificate). Expect to adjust the manifest if Windows
  rejects it; the classic menu (register-shell.ps1) is the tested path.

  For a release, sign the package with a real code-signing certificate and ship the .msix.

.PARAMETER DllPath
  diffsync_shell.dll. Its folder becomes the package's external location.

.PARAMETER Unregister
  Removes the package.

.PARAMETER DeveloperMode
  Register the manifest directly (Add-AppxPackage -Register): needs Windows Developer Mode, no
  signing, no SDK.

.EXAMPLE
  .\register-modern-menu.ps1 -DllPath C:\Tools\DiffSync\diffsync_shell.dll
  .\register-modern-menu.ps1 -Unregister
#>
[CmdletBinding()]
param(
    [string] $DllPath,
    [switch] $Unregister,
    [switch] $DeveloperMode
)

$ErrorActionPreference = 'Stop'
$PackageName = 'DiffSync.ShellExtension'
$Root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

if ($Unregister) {
    Get-AppxPackage -Name $PackageName | Remove-AppxPackage
    Write-Host 'The package is removed.'
    return
}

if (-not $DllPath) { $DllPath = Join-Path $Root 'dist\x64\diffsync_shell.dll' }
if (-not (Test-Path $DllPath)) { throw "diffsync_shell.dll was not found at $DllPath: build it with scripts\build-core.ps1 -Shell." }
$external = Split-Path -Parent (Resolve-Path $DllPath).Path

$stage = Join-Path ([IO.Path]::GetTempPath()) "diffsync-sparse-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path "$stage\Assets" | Out-Null
Copy-Item (Join-Path $Root 'shell\AppxManifest.xml') "$stage\AppxManifest.xml"

# the manifest names logo files: write plain placeholders (replace them with real artwork)
Add-Type -AssemblyName System.Drawing
foreach ($logo in @(@('StoreLogo.png', 50), @('Square44x44Logo.png', 44), @('Square150x150Logo.png', 150))) {
    $bmp = New-Object System.Drawing.Bitmap $logo[1], $logo[1]
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 30, 120, 200))
    $g.Dispose()
    $bmp.Save("$stage\Assets\$($logo[0])", [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

if ($DeveloperMode) {
    Add-AppxPackage -Register "$stage\AppxManifest.xml" -ExternalLocation $external
    Write-Host 'Registered in developer mode. Restart Explorer (or sign out) to see the entries.'
    return
}

$sdk = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | ForEach-Object { Join-Path $_.FullName 'x64' } |
    Where-Object { Test-Path "$_\makeappx.exe" } | Select-Object -First 1
if (-not $sdk) { throw 'The Windows SDK (makeappx.exe, signtool.exe) was not found. Install it, or use -DeveloperMode.' }

# a self-signed certificate whose subject equals the manifest's Publisher
$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq 'CN=DiffSync' } | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type Custom -Subject 'CN=DiffSync' -KeyUsage DigitalSignature `
        -FriendlyName 'DiffSync shell extension' -CertStoreLocation 'Cert:\CurrentUser\My' `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
}
$cer = Join-Path $stage 'diffsync.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Write-Host 'Trusting the certificate needs administrator rights (once):'
Start-Process powershell -Verb RunAs -Wait -ArgumentList "-NoProfile -Command Import-Certificate -FilePath '$cer' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"

$msix = Join-Path $stage 'diffsync-shell.msix'
& "$sdk\makeappx.exe" pack /d $stage /p $msix /nv | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'makeappx failed' }
& "$sdk\signtool.exe" sign /fd SHA256 /sha1 $cert.Thumbprint $msix | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'signtool failed' }

Add-AppxPackage -Path $msix -ExternalLocation $external
Write-Host "Registered. Restart Explorer (or sign out) to see the entries in the Windows 11 menu."
