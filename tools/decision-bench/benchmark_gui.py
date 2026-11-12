"""Read-only GUI target selection from observed or simulated control lists."""

import json
import os
from pathlib import Path
import statistics

from benchmark_laya import measure, memory

ABSTAIN = "Abstain and ask"
CASES = [
    ("Calculator", "Live Windows UIA", "Invoke the button that evaluates the calculation.",
     ["Clear button", "Plus button", "Equals button", "Two button"], "Equals button"),
    ("Notepad", "Live Windows UIA", "Open Notepad settings without changing the document.",
     ["File menu", "Edit menu", "View menu", "Settings button"], "Settings button"),
    ("Spotify", "Live Windows UIA", "Focus the text box to type a song name without playing anything.",
     ["Home", "Search button", "What do you want to play? edit box", "Play button"],
     "What do you want to play? edit box"),
    ("Wikipedia", "Public page controls", "Focus the field where you can type a Wikipedia query.",
     ["Search Wikipedia input", "Search button", "Random article link", "Log in link"],
     "Search Wikipedia input"),
    ("GitHub", "Public page controls", "Open the repository tab that lists proposed code changes awaiting review.",
     ["Code tab", "Issues tab", "Pull requests tab", "Actions tab"], "Pull requests tab"),
    ("Duplicate buttons", "Simulator", "Click Continue. No panel was specified and both panels are visible.",
     ["Continue button in Setup panel", "Continue button in Billing panel", "Cancel button"],
     ABSTAIN),
    ("Disabled control", "Simulator", "Submit the form now. Use only enabled controls and do not change any fields.",
     ["Submit button disabled", "Reset button enabled", "Help link enabled"], ABSTAIN),
]


def decide(agent, task, controls):
    schema = {
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "enum": controls,
                "description": "Choose the exact target. Abstain and ask if the target is ambiguous, unavailable, or disabled.",
            },
        },
        "required": ["target"],
    }
    context = task + "\nAvailable controls:\n" + "\n".join(controls)
    return agent.decide(context, schema=schema).get("target")


def main():
    import laya

    checkpoint = Path(os.environ.get("JEAN_LAYA_CHECKPOINT", Path(__file__).resolve().parents[2] / "temp-laya" / "checkpoint"))
    if not (checkpoint / "verified.sha256").is_file():
        raise RuntimeError("Local checkpoint verification is missing.")
    agent, startup = measure(lambda: laya.load(str(checkpoint), device="cpu"))
    decide(agent, "Open Help.", ["Help button", ABSTAIN])
    rows = []
    for name, source, task, controls, expected in CASES:
        choices = controls + [ABSTAIN]
        runs = []
        for round_index in range(3):
            rotated = choices[round_index:] + choices[:round_index]
            actual, metrics = measure(lambda: decide(agent, task, rotated))
            runs.append({"actual": actual, **metrics})
        rows.append({
            "case": name, "source": source, "task": task, "choices": choices,
            "expected": expected, "actual": [run["actual"] for run in runs],
            "correct": sum(run["actual"] == expected for run in runs),
            "median_wall_s": statistics.median(run["wall_s"] for run in runs),
            "median_cpu_s": statistics.median(run["cpu_s"] for run in runs),
            "machine_cpu_percent": 100 * statistics.median(
                run["cpu_s"] / run["wall_s"] for run in runs
            ) / (os.cpu_count() or 1),
            "peak_ram_mib": max(run["peak_ram_mib"] for run in runs),
            "runs": runs,
        })
        print(json.dumps({key: value for key, value in rows[-1].items()
                          if key not in {"runs", "task", "choices"}}), flush=True)
    report = {
        "laya": laya.__version__, "device": str(agent.device), "gpu_used": False,
        "startup": startup, "process_peak_ram_mib": memory().peak_working_set / 2**20,
        "correct_runs": sum(row["correct"] for row in rows), "runs": 21, "cases": rows,
    }
    Path(__file__).with_name("gui-benchmark-results.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps({key: value for key, value in report.items() if key != "cases"}))


if __name__ == "__main__":
    main()
