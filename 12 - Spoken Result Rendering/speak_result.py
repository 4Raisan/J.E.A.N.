"""Render a trusted task result with a local Ollama model, then speak it."""
import argparse
import json
from pathlib import Path
import subprocess
import urllib.request

from render_prompt import _delegate_result_prompt

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("result", type=Path)
parser.add_argument("--model", required=True, help="Installed local Ollama model tag")
parser.add_argument("--failed", action="store_true")
args = parser.parse_args()
with args.result.open(encoding="utf-8-sig") as source:
    result = source.read(100001).strip()
if not result or len(result) > 100000:
    parser.error("Expected a nonempty result of at most 100,000 characters")
prompt = _delegate_result_prompt(result, language="en", success=not args.failed)
request = urllib.request.Request(
    "http://127.0.0.1:11434/api/chat",
    data=json.dumps({
        "model": args.model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": False,
        "options": {"num_predict": 180},
    }).encode(),
    headers={"Content-Type": "application/json"},
)
try:
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=90) as response:
        spoken = json.load(response)["message"]["content"].strip()
except (OSError, ValueError, KeyError) as exc:
    raise SystemExit(f"Local rendering failed: {exc}") from exc
if not spoken:
    raise SystemExit("Local model returned no spoken reply")
print(spoken, flush=True)
speech = Path(__file__).resolve().parent.parent / "9 - Local Whisper and Windows Speech" / "speech.py"
import sys

subprocess.run([sys.executable, str(speech), "--speak", spoken], check=True)
