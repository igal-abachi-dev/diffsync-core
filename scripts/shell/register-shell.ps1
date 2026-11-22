<#
.SYNOPSIS
  Registers (or removes) the DiffSync Explorer context-menu entries for the current user.

.DESCRIPTION
  Adds "Select left side" and "Compare with <left>" to the right-click menu of every file and
  folder. Everything goes under HKCU\Software\Classes: no administrator rights, nothing machine
  wide, and -Unregister removes exactly what was added.

  This is the classic menu: on Windows 11 it appears under "Show more options" (Shift+F10). For the
  new top-level Windows 11 menu use register-modern-menu.ps1 (it needs a signed sparse package).

  Explorer keeps diffsync_shell.dll loaded. To replace the DLL, run with -Unregister -RestartExplorer
  first (or sign out), copy the new DLL, register again.

.PARAMETER DllPath
  diffsync_shell.dll. Default: next to this script's dist folder (..\..\dist\x64) or the release
  build in target\release.

.PARAMETER Command
  The compare tool command line (`%L` is the left side, `%R` the right side), stored in
  HKCU\Software\DiffSync\Shell\Command. Default: DeltaForge.exe next to the DLL.
  Example: '"C:\Program Files\DeltaForge\DeltaForge.exe" "%L" "%R"'

.PARAMETER Unregister
  Removes the entries (and the COM registration) instead of adding them.

.PARAMETER RestartExplorer
  Restarts Explorer so a changed or removed DLL is released. Windows starts it again by itself.

.EXAMPLE
  .\register-shell.ps1 -DllPath C:\Tools\DiffSync\diffsync_shell.dll -Command '"C:\Tools\DeltaForge.exe" "%L" "%R"'
  .\register-shell.ps1 -Unregister -RestartExplorer
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string] $DllPath,
    [string] $Command,
    [switch] $Unregister,
    [switch] $RestartExplorer
)

$ErrorActionPreference = 'Stop'

# The CLSIDs of crates/shell-ext (they are part of the DLL: change them there too).
$Entries = @(
    @{ Clsid = '{B3F2A1C0-5D7E-4C8A-9E21-0A6D3F4B7C11}'; Verb = 'DiffSync.SelectLeft';          Name = 'DiffSync: Select left side' },
    @{ Clsid = '{B3F2A1C0-5D7E-4C8A-9E21-0A6D3F4B7C12}'; Verb = 'DiffSync.CompareWithSelected'; Name = 'DiffSync: Compare with selected' }
)
$Classes = 'HKCU:\Software\Classes'
# every file and every folder
$VerbRoot = "$Classes\AllFilesystemObjects\shell"

function Notify-Shell {
    # tell Explorer that file associations changed so the menu is rebuilt
    Add-Type -Namespace DiffSync -Name Native -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("shell32.dll")]
public static extern void SHChangeNotify(int eventId, uint flags, System.IntPtr item1, System.IntPtr item2);
'@
    [DiffSync.Native]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
}

if ($Unregister) {
    foreach ($e in $Entries) {
        foreach ($key in @("$VerbRoot\$($e.Verb)", "$Classes\CLSID\$($e.Clsid)")) {
            if (Test-Path $key) {
                if ($PSCmdlet.ShouldProcess($key, 'remove')) { Remove-Item -Recurse -Force $key }
            }
        }
    }
    if (Test-Path 'HKCU:\Software\DiffSync\Shell') {
        if ($PSCmdlet.ShouldProcess('HKCU:\Software\DiffSync\Shell', 'remove')) { Remove-Item -Recurse -Force 'HKCU:\Software\DiffSync\Shell' }
    }
    Write-Host 'The DiffSync context-menu entries are removed.'
}
else {
    if (-not $DllPath) {
        $root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
        $candidates = @("$root\dist\x64\diffsync_shell.dll", "$root\target\release\diffsync_shell.dll", "$root\target\debug\diffsync_shell.dll")
        $DllPath = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $DllPath -or -not (Test-Path $DllPath)) {
        throw 'diffsync_shell.dll was not found: pass -DllPath, or build it with scripts\build-core.ps1 -Shell.'
    }
    $DllPath = (Resolve-Path $DllPath).Path

    foreach ($e in $Entries) {
        if ($PSCmdlet.ShouldProcess($e.Name, "register $DllPath")) {
            # the COM class: an in-process server in this DLL
            $clsKey = "$Classes\CLSID\$($e.Clsid)"
            New-Item -Path $clsKey -Force | Out-Null
            Set-ItemProperty -Path $clsKey -Name '(Default)' -Value $e.Name
            New-Item -Path "$clsKey\InprocServer32" -Force | Out-Null
            Set-ItemProperty -Path "$clsKey\InprocServer32" -Name '(Default)' -Value $DllPath
            Set-ItemProperty -Path "$clsKey\InprocServer32" -Name 'ThreadingModel' -Value 'Apartment'
            # the menu entry: Explorer asks the COM class for the title and the state
            $verbKey = "$VerbRoot\$($e.Verb)"
            New-Item -Path $verbKey -Force | Out-Null
            Set-ItemProperty -Path $verbKey -Name 'ExplorerCommandHandler' -Value $e.Clsid
        }
    }
    if ($Command) {
        if ($PSCmdlet.ShouldProcess('HKCU:\Software\DiffSync\Shell\Command', 'set')) {
            New-Item -Path 'HKCU:\Software\DiffSync\Shell' -Force | Out-Null
            Set-ItemProperty -Path 'HKCU:\Software\DiffSync\Shell' -Name 'Command' -Value $Command
        }
    }
    Write-Host "Registered for the current user: $DllPath"
    Write-Host 'Right-click a file or folder; on Windows 11 choose "Show more options" (or press Shift+F10).'
}

if ($PSCmdlet.ShouldProcess('Explorer', 'notify')) { Notify-Shell }
if ($RestartExplorer -and $PSCmdlet.ShouldProcess('explorer.exe', 'restart')) {
    Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
}
