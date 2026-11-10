@echo off
cd /d "%~dp0"
uv run --offline --python 3.12 --no-project --with sherpa-onnx==2.0.1 --with sounddevice==0.5.6 --with numpy==2.5.4 python -B dictate.py %*
set "result=%errorlevel%"
if "%~1"=="" pause
exit /b %result%
