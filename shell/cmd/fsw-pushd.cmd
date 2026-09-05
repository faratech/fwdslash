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
if not defined TEMP goto native

rem See fsw-cd.cmd: FOR /F cannot report the child's exit code, so the
rem controller's stdout line goes through a temp file.
set "fsw_out=%TEMP%\fsw-pushd-%RANDOM%%RANDOM%.tmp"
"%~dp0fwdslash.exe" cmd-cd "%fsw_target%" >"%fsw_out%"
set "fsw_code=%errorlevel%"
set "fsw_path="
for /f "usebackq delims=" %%T in ("%fsw_out%") do set "fsw_path=%%T"
del "%fsw_out%" >nul 2>&1
if "%fsw_code%"=="3" goto native
if not "%fsw_code%"=="0" exit /b 1
if "%fsw_path%"=="" exit /b 1
endlocal & pushd "%fsw_path%"
goto :eof

:native
endlocal & pushd %*
