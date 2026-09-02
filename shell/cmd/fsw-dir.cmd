@echo off
setlocal DisableDelayedExpansion

rem Keep ordinary DIR syntax native. Only a single slash-alias argument is
rem translated, so switches such as DIR /A retain their original behavior.
if "%~1"=="" goto native
if not "%~2"=="" goto native
if "%~1"=="/" goto wslroot

set "fsw_alias=%~1"
if not "%fsw_alias:~0,1%"=="/" goto native
"%~dp0fswctl.exe" cmd-list "%fsw_alias%"
if errorlevel 3 goto native
exit /b %errorlevel%

:wslroot
endlocal & "%~dp0fswctl.exe" list /
exit /b %errorlevel%

:native
endlocal & dir %*
