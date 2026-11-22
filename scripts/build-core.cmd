@echo off
rem Builds diffsync_core.dll (and, with -Shell, the Explorer shell extension) into dist\.
rem A thin wrapper so it works from cmd.exe and by double click; the work is in build-core.ps1.
rem
rem   scripts\build-core.cmd                    x64 release DLL into dist\x64
rem   scripts\build-core.cmd -Arch all -Shell   x64 and ARM64, with the shell extension
rem   scripts\build-core.cmd -Test              run the tests first
rem
rem Every argument is passed on to the PowerShell script (see its help: Get-Help .\scripts\build-core.ps1).
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-core.ps1" %*
set RC=%ERRORLEVEL%
if not "%RC%"=="0" (
  echo.
  echo Build failed with exit code %RC%.
)
exit /b %RC%
