@echo off
setlocal DisableDelayedExpansion

rem Keep ordinary DIR syntax native. Only a single slash-alias argument is
rem translated, so switches such as DIR /A retain their original behavior.
if "%~1"=="" goto native
if not "%~2"=="" goto native
set "fsw_alias=%~1"
if not "%fsw_alias:~0,1%"=="/" goto native
rem DIR switches start with "/" too: /a /b /s /w /? /a:d /o:n. Anything whose
rem third character is empty or ":" is a switch, not a distribution path --
rem decided here, before the spawn, because a bare-slash default distribution
rem would otherwise resolve /b to a real UNC path and boot WSL to list it.
rem A one-letter distribution name (/X) is knowingly unsupported. Bare "/" is
rem the exception: it is never a DIR switch, and listing the distributions is
rem the whole point of the adapter.
if "%fsw_alias%"=="/" goto resolve
if "%fsw_alias:~2,1%"=="" goto native
if "%fsw_alias:~2,1%"==":" goto native

:resolve
"%~dp0fwdslash.exe" cmd-list "%fsw_alias%"
if errorlevel 3 goto native
exit /b %errorlevel%

:native
endlocal & dir %*
