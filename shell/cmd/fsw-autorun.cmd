@echo off
doskey dir=call "%~dp0fsw-dir.cmd" $*
doskey ls=call "%~dp0fsw-dir.cmd" $*
doskey cd=call "%~dp0fsw-cd.cmd" $*
doskey chdir=call "%~dp0fsw-cd.cmd" $*
doskey pushd=call "%~dp0fsw-pushd.cmd" $*
