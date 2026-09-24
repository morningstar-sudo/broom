@echo off
rem Double-click build on Windows: runs build.sh inside WSL (distro Ubuntu-24.04; override with BROOM_WSL).
rem Extra options pass through, e.g.  build.cmd --ipxe   /   build.cmd --no-test
cd /d "%~dp0"
if "%BROOM_WSL%"=="" set BROOM_WSL=Ubuntu-24.04
wsl -d %BROOM_WSL% -- bash ./build.sh %*
if errorlevel 1 (echo. & echo BUILD FAILED) else (echo. & echo BUILD OK)
pause
