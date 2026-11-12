"""Run the saved JEAN cases against Inception Mercury."""

import argparse
import json
import os
from pathlib import Path
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

import benchmark_gui as gui
import benchmark_laya as routing

URL = "https://api.inceptionlabs.ai/v1/chat/completions"
CELERIS_URL = "https://inference.celeris.ai/celeris-1/v1/chat/completions"


def read_stream(response, start):
    parts = []
    first_text = None
    usage = None
    for raw in response:
        line = raw.decode("utf-8").strip()
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            return "".join(parts).strip(), time.perf_counter() - start, usage, first_text
        event = json.loads(data)
        usage = event.get("usage") or usage
        for choice in event.get("choices", []):
            content = choice.get("delta", {}).get("content")
            if content:
                if first_text is None:
                    first_text = time.perf_counter() - start
                parts.append(content)
    raise ValueError("Celeris stream ended without a completion marker")


def choose(api_key, model, task, choices, policy):
    is_decide = model == "mercury-decide"
    is_celeris = model == "celeris-1"
    if is_decide:
        payload = {
            "model": model,
            "state": task,
            "questions": {"target": {
                "type": "choice",
                "instructions": policy,
                "criteria": {option: option for option in choices},
            }},
        }
    else:
        payload = {
            "model": model,
            "messages": [
                {"role": "system", "content": policy + " Return only one exact option."},
                {"role": "user", "content": json.dumps({"task": task, "options": choices})},
            ],
        }
    endpoint = URL
    if is_decide:
        endpoint = "https://api.inceptionlabs.ai/v1/decisions"
    elif is_celeris:
        endpoint = CELERIS_URL
        payload.update(max_tokens=2048, stream=True, stream_options={"include_usage": True})
    request = Request(
        endpoint,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Authorization": "Bearer " + api_key, "Content-Type": "application/json"},
    )
    start = time.perf_counter()
    with urlopen(request, timeout=45) as response:
        if is_celeris:
            return read_stream(response, start)
        result = json.load(response)
    elapsed = time.perf_counter() - start
    actual = (result["answers"]["target"]["choice"] if is_decide
              else result["choices"][0]["message"]["content"].strip())
    return actual, elapsed, result.get("usage"), None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=("routing", "gui", "both"), default="both")
    parser.add_argument("--repeats", type=int, choices=(1, 2, 3), default=1)
    parser.add_argument("--model", default="mercury-3")
    parser.add_argument("--output", type=Path, default=Path("mercury-results.json"))
    args = parser.parse_args()
    key_name = "CELERIS_API_KEY" if args.model == "celeris-1" else "INCEPTION_API_KEY"
    key = os.environ.get(key_name)
    if not key and key_name == "INCEPTION_API_KEY":
        key = os.environ.get("MERCURY_API_KEY")
    if not key:
        parser.error(f"Set {key_name} in the Windows environment before running.")

    cases = []
    if args.suite in ("routing", "both"):
        policy = "Select the best route. " + " ".join(
            f"{name}: {rule}" for name, rule in routing.ROUTES.items()
        )
        cases += [("routing", name, task, list(routing.ROUTES), expected, policy)
                  for name, expected, task in routing.CASES]
    if args.suite in ("gui", "both"):
        policy = "Select the exact control. Abstain and ask if it is ambiguous, unavailable, or disabled."
        cases += [("gui", name, task, controls + [gui.ABSTAIN], expected, policy)
                  for name, _, task, controls, expected in gui.CASES]

    rows = []
    for suite, name, task, choices, expected, policy in cases:
        for repeat in range(args.repeats):
            rotated = choices[repeat:] + choices[:repeat] if suite == "gui" else choices
            try:
                actual, elapsed, usage, first_text = choose(key, args.model, task, rotated, policy)
            except HTTPError as error:
                raise SystemExit(f"{args.model} request failed with HTTP {error.code}") from None
            except URLError as error:
                raise SystemExit(f"{args.model} connection failed: {error.reason}") from None
            row = {
                "suite": suite, "case": name, "repeat": repeat + 1, "expected": expected,
                "actual": actual, "correct": actual == expected, "wall_s": elapsed, "usage": usage,
            }
            if first_text is not None:
                row["first_text_s"] = first_text
            rows.append(row)
            print(json.dumps(row), flush=True)
    report = {"model": args.model, "correct": sum(row["correct"] for row in rows),
              "total": len(rows), "runs": rows}
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"Correct: {report['correct']}/{report['total']}")


if __name__ == "__main__":
    main()
