"""Seven local JEAN routing cases. Run with the Laya virtual environment."""

import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import statistics
import threading
import time

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"

ROUTES = {
    "Codex Medium": "Small or well-scoped code edits and focused tests.",
    "Codex High": "Complex architecture, concurrency, or multi-component debugging.",
    "Claude Code": "Coding in a workspace whose required tools are available only to Claude Code.",
    "Windows UI Automation": "Act on an explicitly identified Windows control through its accessibility tree.",
    "Research": "Find or compare information and provide sources.",
    "Chat": "Conversation or explanation without an external action.",
    "Clarify": "Ask a question when a consequential action has an ambiguous target.",
}
SCHEMA = {
    "type": "object",
    "properties": {
        "route": {
            "type": "string",
            "enum": list(ROUTES),
            "description": "Select the best eligible route. " + " ".join(
                f"{name}: {rule}" for name, rule in ROUTES.items()
            ),
        }
    },
    "required": ["route"],
}

CASES = [
    ("focused fix", "Codex Medium",
     "Fix an off-by-one error in one Rust function and run its existing unit test."),
    ("cross-crate race", "Codex High",
     "Diagnose a rare cancellation race spanning the supervisor, process worker, and SQLite recovery. Preserve the no-replay guarantee."),
    ("worker capability", "Claude Code",
     "Implement a code fix using the workspace's required test connector. The connector is available to Claude Code only."),
    ("named desktop control", "Windows UI Automation",
     "In the uniquely identified Notepad window, invoke the Find control with AutomationId FindButton."),
    ("source comparison", "Research",
     "Find primary sources for Windows UI Automation and compare supported control patterns."),
    ("explanation only", "Chat",
     "Explain in plain English what a checkpoint is and why recovery uses it."),
    ("ambiguous action", "Clarify",
     "Close the editor window. Two matching editor windows are open, both with unsaved changes, and no target was specified."),
]


class MemoryCounters(ctypes.Structure):
    _fields_ = [
        ("size", wintypes.DWORD),
        ("page_fault_count", wintypes.DWORD),
        ("peak_working_set", ctypes.c_size_t),
        ("working_set", ctypes.c_size_t),
        ("peak_paged_pool", ctypes.c_size_t),
        ("paged_pool", ctypes.c_size_t),
        ("peak_nonpaged_pool", ctypes.c_size_t),
        ("nonpaged_pool", ctypes.c_size_t),
        ("pagefile", ctypes.c_size_t),
        ("peak_pagefile", ctypes.c_size_t),
    ]


kernel = ctypes.WinDLL("kernel32", use_last_error=True)
kernel.GetCurrentProcess.restype = wintypes.HANDLE
kernel.K32GetProcessMemoryInfo.argtypes = (
    wintypes.HANDLE, ctypes.POINTER(MemoryCounters), wintypes.DWORD
)
kernel.K32GetProcessMemoryInfo.restype = wintypes.BOOL


def memory():
    counters = MemoryCounters()
    counters.size = ctypes.sizeof(counters)
    if not kernel.K32GetProcessMemoryInfo(
        kernel.GetCurrentProcess(), ctypes.byref(counters), counters.size
    ):
        raise ctypes.WinError(ctypes.get_last_error())
    return counters


def measure(call):
    stop = threading.Event()
    peak = memory().working_set

    def sample():
        nonlocal peak
        while not stop.wait(0.02):
            peak = max(peak, memory().working_set)

    sampler = threading.Thread(target=sample)
    sampler.start()
    start_wall, start_cpu = time.perf_counter(), time.process_time()
    try:
        result = call()
    finally:
        wall = time.perf_counter() - start_wall
        cpu = time.process_time() - start_cpu
        stop.set()
        sampler.join()
    peak = max(peak, memory().working_set)
    return result, {"wall_s": wall, "cpu_s": cpu, "peak_ram_mib": peak / 2**20}


def main():
    import laya
    import torch

    checkpoint = Path(os.environ.get("JEAN_LAYA_CHECKPOINT", Path(__file__).resolve().parents[2] / "temp-laya" / "checkpoint"))
    if not (checkpoint / "verified.sha256").is_file():
        raise RuntimeError("Local checkpoint verification is missing.")
    agent, startup = measure(lambda: laya.load(str(checkpoint), device="cpu"))
    print("Model startup:", json.dumps(startup), flush=True)
    agent.decide("Say hello to the user.", schema=SCHEMA)

    results = {name: [] for name, _, _ in CASES}
    for round_index in range(3):
        for name, expected, task in CASES[round_index:] + CASES[:round_index]:
            decision, metrics = measure(lambda: agent.decide(task, schema=SCHEMA))
            results[name].append({"expected": expected, "actual": decision.get("route"), **metrics})

    summary = []
    for name, runs in results.items():
        wall = statistics.median(run["wall_s"] for run in runs)
        cpu = statistics.median(run["cpu_s"] for run in runs)
        summary.append({
            "case": name,
            "expected": runs[0]["expected"],
            "actual": [run["actual"] for run in runs],
            "correct": sum(run["expected"] == run["actual"] for run in runs),
            "wall_s": round(wall, 3),
            "cpu_s": round(cpu, 3),
            "cpu_pct_one_core": round(100 * statistics.median(
                run["cpu_s"] / run["wall_s"] for run in runs
            ), 1),
            "peak_ram_mib": round(max(run["peak_ram_mib"] for run in runs), 1),
        })
    report = {
        "laya": laya.__version__,
        "torch": torch.__version__,
        "device": str(agent.device),
        "logical_processors": os.cpu_count(),
        "torch_threads": torch.get_num_threads(),
        "gpu_used": agent.device.type != "cpu",
        "startup": startup,
        "process_peak_ram_mib": memory().peak_working_set / 2**20,
        "summary": summary,
        "runs": results,
    }
    path = Path(__file__).with_name("benchmark-results.json")
    path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: value for key, value in report.items() if key != "runs"}, indent=2))


if __name__ == "__main__":
    main()
