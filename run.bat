@echo off
setlocal
cd /d "%~dp0"

if not exist target\release\server.exe (
    echo server.exe not found - run update.bat first.
    exit /b 1
)

rem Closing this window stops the server (graceful shutdown).
target\release\server.exe --bind 127.0.0.1:64500 --data "%USERPROFILE%\.nightingale-fork"
