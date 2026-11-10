@echo off
setlocal
cd /d "%~dp0"
if not exist "target\debug\jean.exe" (
    echo JEAN's local build is missing from target\debug.
    pause
    exit /b 1
)

echo JEAN local console
echo Type /help for commands, /status for session state, or /quit to exit.
echo Voice and model-backed replies require a configured backend.
echo.
"target\debug\jean.exe" interact --tty
if errorlevel 1 pause
