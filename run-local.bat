@echo off
setlocal
cd /d "%~dp0"

echo [1/3] Building client (embedded into server)...
pushd client
call pnpm build
if errorlevel 1 goto :fail
popd

echo [2/3] Compiling server (release, incremental)...
cargo build --release -p server
if errorlevel 1 goto :fail

echo [3/3] Starting self-hosted server on http://127.0.0.1:64500
echo Data folder: %USERPROFILE%\.nightingale-fork
target\release\server.exe --bind 127.0.0.1:64500 --data "%USERPROFILE%\.nightingale-fork"
if errorlevel 1 goto :fail
goto :eof

:fail
echo.
echo Build/start failed - see output above.
exit /b 1
