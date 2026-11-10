@echo off
setlocal
set "JEAN_SCRIPT=%~dp0tools\jean-desktop\jean-desktop.py"
"%SystemRoot%\py.exe" -3 "%JEAN_SCRIPT%"
if errorlevel 1 pause
