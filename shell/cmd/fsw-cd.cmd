@echo off
rem Forward Slash Windows: the CD/CHDIR adapter.
rem
rem Only "cd /<slash path>" and "cd /d /<slash path>" are translated. Every
rem other form -- cd, cd .., cd /?, cd /d, cd C:\..., cd /d C:\... -- runs the
rem native verb untouched. fsw-pushd.cmd is this script with PUSHD as its
rem native fallback verb; keep the two in step.
rem
rem The resolved target is entered with PUSHD, not CD: cmd.exe cannot make a
rem UNC path current, and PUSHD maps a temporary drive letter that POPD (or
rem closing the window) releases. That directory change has to outlive this
rem script, so it runs on an "endlocal &" line rather than inside SETLOCAL.
setlocal DisableDelayedExpansion

if "%~1"=="" goto native
if not "%~3"=="" goto native
if not exist "%~dp0fwdslash.exe" goto native
if "%~2"=="" goto single
rem Two arguments qualify only as "/d <slash path>".
if /i not "%~1"=="/d" goto native
set "fsw_target=%~2"
goto shape

:single
set "fsw_target=%~1"

:shape
if not "%fsw_target:~0,1%"=="/" goto native
rem "/d" and "/?" are CD switches, not distribution paths -- the same
rem third-character test fsw-dir.cmd uses for DIR's switches. A one-letter
rem distribution name is knowingly unsupported.
rem Bare "/" is the exception: never a switch, and the one input the
rem distribution-list message exists for.
if "%fsw_target%"=="/" goto resolve
if "%fsw_target:~2,1%"=="" goto native
if "%fsw_target:~2,1%"==":" goto native

:resolve
if not defined TEMP goto native

rem FOR /F cannot report the child's exit code, and the exit code is what
rem separates "run your own CD" (3) from "the resolver said no" (1), so the
rem controller's single stdout line goes through a temp file. Its stderr is
rem left alone: that is where the user-facing message appears.
set "fsw_out=%TEMP%\fsw-cd-%RANDOM%%RANDOM%.tmp"
"%~dp0fwdslash.exe" cmd-cd "%fsw_target%" >"%fsw_out%"
set "fsw_code=%errorlevel%"
set "fsw_path="
for /f "usebackq delims=" %%T in ("%fsw_out%") do set "fsw_path=%%T"
del "%fsw_out%" >nul 2>&1
if "%fsw_code%"=="3" goto native
rem Exit 1 has already explained itself on stderr. An empty capture cannot
rem happen on exit 0, but running CD on "/etc" would be the worse guess.
if not "%fsw_code%"=="0" exit /b 1
if "%fsw_path%"=="" exit /b 1
endlocal & pushd "%fsw_path%"
goto :eof

:native
endlocal & cd %*
