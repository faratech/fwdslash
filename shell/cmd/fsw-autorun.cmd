@echo off
rem Forward Slash Windows: the cmd AutoRun hook.
rem
rem This file is a TEMPLATE. The installer (`fwdslash integration cmd enable`)
rem GENERATES the deployed copy in %LOCALAPPDATA%\ForwardSlashWindows\cmd with
rem the product-presence probe baked in, so the packaged and unpackaged flavors
rem each test the right path (#37). The probe below is the packaged default.
rem
rem While the product is present the doskey macros are installed; when it is
rem gone (an MSIX uninstall runs no code) the macros are NOT installed -- so
rem `dir /etc` falls through to native cmd instead of an orphaned controller --
rem and the staged controller's self-clean runs once to remove the leftovers.
if not exist "%LOCALAPPDATA%\Microsoft\WindowsApps\fwdslash.exe" goto fsw_gone
doskey dir=call "%~dp0fsw-dir.cmd" $*
doskey ls=call "%~dp0fsw-dir.cmd" $*
doskey cd=call "%~dp0fsw-cd.cmd" $*
doskey chdir=call "%~dp0fsw-cd.cmd" $*
doskey pushd=call "%~dp0fsw-pushd.cmd" $*
goto :eof
:fsw_gone
if exist "%~dp0fwdslash.exe" start "" /b "%~dp0fwdslash.exe" uninstall --orphaned >nul 2>&1
