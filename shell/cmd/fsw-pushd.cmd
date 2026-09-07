@echo off
rem Forward Slash Windows: the PUSHD adapter.
rem
rem fsw-cd.cmd with PUSHD as the native fallback verb -- keep the two in step.
rem Only "pushd /<slash path>" (and, for symmetry with CD, "/d <slash path>")
rem is translated; every other form runs the native verb untouched.
setlocal DisableDelayedExpansion

if "%~1"=="" goto native
if not "%~3"=="" goto native
if not exist "%~dp0fwdslash.exe" goto native
if "%~2"=="" goto single
if /i not "%~1"=="/d" goto native
set "fsw_target=%~2"
goto shape

:single
set "fsw_target=%~1"

:shape
if not "%fsw_target:~0,1%"=="/" goto native
rem A leading-slash argument whose third character is empty or ":" is a
rem switch, not a distribution path (see fsw-cd.cmd).
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
endlocal & pushd %*
