# JEAN decision benchmarks

The two suites preserve the seven task-routing cases and seven GUI
selection cases from the Laya trial. Each has a fixed expected answer.
The GUI cases use saved, minimal control lists from Calculator, Notepad,
Spotify, Wikipedia, GitHub, and two synthetic examples. The suite does
not open or click apps, capture screens, or run any selected action.

Run from the JEAN root in Windows Command Prompt:

```bat
temp-laya\.venv\Scripts\python.exe -B tools\decision-bench\benchmark_laya.py
temp-laya\.venv\Scripts\python.exe -B tools\decision-bench\benchmark_gui.py
```

These use the verified local Laya checkpoint under `temp-laya\checkpoint`
by default. Set `JEAN_LAYA_CHECKPOINT` to select another verified checkpoint.
Both report three repetitions, correctness, timing, CPU and RAM.

For Mercury, set `INCEPTION_API_KEY` in your Windows environment using
your preferred secret manager, then open a Command Prompt that inherits
it. Start with one request per case:

```bat
py -3 -B tools\decision-bench\benchmark_mercury.py --model mercury-3 --suite both --output "%TEMP%\jean-mercury-3-results.json"
py -3 -B tools\decision-bench\benchmark_mercury.py --model mercury-decide --suite both --output "%TEMP%\jean-mercury-decide-results.json"
```

Use `--repeats 3` to match Laya's repeat count. This sends only the saved
test prompts and options to Inception and records API latency and token
usage. The hosted model's CPU, RAM and GPU usage are unavailable to this
client. Laya and Mercury Decide use typed choices. Mercury 3 uses chat
completions with an instruction to return one exact label.

For Celeris, set `CELERIS_API_KEY` in the Windows environment and run:

```bat
py -3 -B tools\decision-bench\benchmark_mercury.py --model celeris-1 --output "%TEMP%\jean-celeris-1-results.json"
```

Celeris records the time to first visible text and to stream completion.
