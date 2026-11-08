@echo off
setlocal
rem Double-click launcher for the JEAN voice tester, real microphone and real speaker.
rem Every path is resolved from this file's own folder (%~dp0), because Explorer starts a
rem double-clicked launcher with a working directory you did not choose.
set "PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%PS%" set "PS=powershell.exe"
"%PS%" -NoProfile -ExecutionPolicy Bypass -STA -WindowStyle Hidden -File "%~dp0voice-tester.ps1"
if errorlevel 1 (
    echo The voice tester exited with a failure. Its message is above this line.
    pause
)
endlocal
