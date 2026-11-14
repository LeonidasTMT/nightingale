@echo off
setlocal
cd /d "%~dp0"

rem Refuse to rebuild over a running server (Windows locks the running exe).
tasklist /FI "IMAGENAME eq server.exe" 2>nul | find /I "server.exe" >nul && (
    echo A server is already running - close its window before updating.
    exit /b 1
)

echo [1/2] Building client...
pushd client
call pnpm build
if errorlevel 1 goto :fail
popd

echo [2/2] Compiling server (release, incremental)...
cargo build --release -p server
if errorlevel 1 goto :fail

echo.
echo Done. Start it with run.bat
goto :eof

:fail
echo Build failed - see output above.
exit /b 1
