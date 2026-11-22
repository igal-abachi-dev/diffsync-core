<#
.SYNOPSIS
  Builds diffsync_core.dll (the C ABI), optionally the Explorer shell extension, and collects
  everything a UI needs in dist/.

.DESCRIPTION
  Does what the developers do by hand:
    1. checks the tools (cargo; Visual Studio build tools are found by rustc itself);
    2. regenerates the C header and the C# bindings and fails if they changed unexpectedly (-Check);
    3. builds core-ffi (and diffsync-shell with -Shell) in release mode for x64, ARM64 or both;
    4. runs the tests with -Test;
    5. copies the DLL, import library, PDB, the header, the C# bindings and a SHA-256 list into
       dist/<arch>/ (and the bindings once into dist/include and dist/csharp).

  The DLL has a static C runtime: no Visual C++ redistributable is needed on the target machine.

.PARAMETER Arch
  x64 (default), arm64 or all. ARM64 needs `rustup target add aarch64-pc-windows-msvc` and the
  Visual Studio ARM64 build tools; the script adds the Rust target when rustup is available.

.PARAMETER Configuration
  Release (default) or Debug.

.PARAMETER Shell
  Also build diffsync_shell.dll (the Windows Explorer context-menu handler).

.PARAMETER Test
  Run `cargo test --workspace` first and stop on a failure.

.PARAMETER Bindings
  Regenerate include/diffsync.h and bindings/csharp (default). `-Bindings:$false` skips it;
  `-CheckBindings` only verifies that they are current (what CI does).

.PARAMETER Dist
  Output folder (default: dist next to this script's parent).

.PARAMETER Clean
  Delete the dist folder first.

.EXAMPLE
  .\scripts\build-core.ps1                       # x64 release DLL into dist\x64
  .\scripts\build-core.ps1 -Arch all -Shell      # x64 and ARM64, with the shell extension
  .\scripts\build-core.ps1 -Test -CheckBindings  # what a release build runs
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64', 'all')] [string] $Arch = 'x64',
    [ValidateSet('Release', 'Debug')] [string] $Configuration = 'Release',
    [switch] $Shell,
    [switch] $Test,
    [bool] $Bindings = $true,
    [switch] $CheckBindings,
    [string] $Dist,
    [switch] $Clean
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
if (-not $Dist) { $Dist = Join-Path $Root 'dist' }

function Step([string] $text) { Write-Host "==> $text" -ForegroundColor Cyan }

function Run([string] $exe, [string[]] $arguments) {
    & $exe @arguments
    if ($LASTEXITCODE -ne 0) { throw "$exe $($arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

Push-Location $Root
try {
    Step 'checking the tools'
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo was not found. Install Rust from https://rustup.rs and open a new terminal.'
    }
    Run 'cargo' @('--version')

    $targets = switch ($Arch) {
        'x64'   { @(@{ Name = 'x64';   Triple = 'x86_64-pc-windows-msvc' }) }
        'arm64' { @(@{ Name = 'arm64'; Triple = 'aarch64-pc-windows-msvc' }) }
        'all'   { @(@{ Name = 'x64'; Triple = 'x86_64-pc-windows-msvc' }, @{ Name = 'arm64'; Triple = 'aarch64-pc-windows-msvc' }) }
    }
    foreach ($t in $targets) {
        if ((Get-Command rustup -ErrorAction SilentlyContinue) -and $t.Triple -ne 'x86_64-pc-windows-msvc') {
            Step "adding the Rust target $($t.Triple)"
            Run 'rustup' @('target', 'add', $t.Triple)
        }
    }

    if ($Clean -and (Test-Path $Dist)) {
        Step "cleaning $Dist"
        Remove-Item -Recurse -Force $Dist
    }

    if ($CheckBindings) {
        Step 'checking that the generated bindings are current'
        Run 'cargo' @('xtask', 'bindings', '--check')
    } elseif ($Bindings) {
        Step 'regenerating include/diffsync.h and bindings/csharp'
        Run 'cargo' @('xtask', 'bindings')
    }

    if ($Test) {
        Step 'running the tests'
        Run 'cargo' @('test', '--workspace', '--features', 'core-ffi/test-panic')
    }

    $profileDir = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
    $buildArgs = @('build', '-p', 'core-ffi')
    if ($Shell) { $buildArgs += @('-p', 'diffsync-shell') }
    if ($Configuration -eq 'Release') { $buildArgs += '--release' }

    foreach ($t in $targets) {
        Step "building $($t.Name) ($($t.Triple), $Configuration)"
        Run 'cargo' ($buildArgs + @('--target', $t.Triple))

        $out = Join-Path $Dist $t.Name
        New-Item -ItemType Directory -Force -Path $out | Out-Null
        $built = Join-Path $Root "target\$($t.Triple)\$profileDir"
        $files = @('diffsync_core.dll', 'diffsync_core.dll.lib', 'diffsync_core.pdb')
        if ($Shell) { $files += @('diffsync_shell.dll', 'diffsync_shell.pdb') }
        foreach ($f in $files) {
            $src = Join-Path $built $f
            if (Test-Path $src) { Copy-Item $src $out -Force }
            elseif ($f -like '*.dll') { throw "the build did not produce $src" }
        }
        # hashes of what was built, so a release can be verified
        $sums = Get-ChildItem $out -File | Where-Object { $_.Extension -in '.dll', '.lib' } |
            ForEach-Object { '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name }
        Set-Content -Path (Join-Path $out 'SHA256SUMS.txt') -Value $sums -Encoding ascii
        Write-Host ("    {0}`n    {1}" -f $out, ($sums -join "`n    "))
    }

    # the header and the C# bindings do not depend on the architecture
    New-Item -ItemType Directory -Force -Path (Join-Path $Dist 'include'), (Join-Path $Dist 'csharp') | Out-Null
    Copy-Item (Join-Path $Root 'include\diffsync.h') (Join-Path $Dist 'include') -Force
    Copy-Item (Join-Path $Root 'bindings\csharp\NativeMethods.g.cs') (Join-Path $Dist 'csharp') -Force
    if ($Shell) {
        Copy-Item (Join-Path $Root 'scripts\shell') (Join-Path $Dist 'shell') -Recurse -Force
        Copy-Item (Join-Path $Root 'shell\AppxManifest.xml') (Join-Path $Dist 'shell') -Force
    }

    Step "done: $Dist"
}
finally {
    Pop-Location
}
