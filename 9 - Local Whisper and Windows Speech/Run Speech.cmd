@echo off
cd /d "%~dp0"
if "%~1"=="" (
  uv run --offline --python 3.12 --no-project --with faster-whisper==1.2.1 --with scipy --with sounddevice python speech.py --mic 5
  pause
  exit /b
)
uv run --python 3.12 --no-project --with faster-whisper==1.2.1 --with scipy --with sounddevice python speech.py %*
