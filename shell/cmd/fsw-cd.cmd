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
rem FOR /F cannot report the child's exit code, so the controller makes its
rem single stdout line self-describing instead (issue #136): a resolved UNC
rem path on success, and ":native" when the caller should run its own verb.
rem A colon cannot begin a UNC path, so the two can never be confused. Nothing
rem touches the filesystem here any more -- this used to write, read and delete
rem a %TEMP% file on every "cd /x" purely to recover the exit code.
rem Its stderr is left alone: that is where the user-facing message appears.
set "fsw_path="
for /f "usebackq delims=" %%T in (`^"^"%~dp0fwdslash.exe^" cmd-cd ^"%fsw_target%^"^"`) do set "fsw_path=%%T"
if "%fsw_path%"==":native" goto native
rem An empty capture is the rejected case, which has already explained itself
rem on stderr. Running the native verb on "/etc" would be the worse guess.
if not defined fsw_path exit /b 1
endlocal & pushd "%fsw_path%"
goto :eof

:native
endlocal & cd %*
