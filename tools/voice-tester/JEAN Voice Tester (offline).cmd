@echo off
setlocal
rem Double-click launcher for the device-free rehearsal: the same window and the same real
rem ONNX engines, but config\offline names a WAV as the microphone and turns the speaker
rem into a counted sink, so nothing is recorded and nothing is audible. Use this first.
set "PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%PS%" set "PS=powershell.exe"
"%PS%" -NoProfile -ExecutionPolicy Bypass -STA -WindowStyle Hidden -File "%~dp0voice-tester.ps1" -Offline
if errorlevel 1 (
    echo The voice tester exited with a failure. Its message is above this line.
    pause
)
endlocal
