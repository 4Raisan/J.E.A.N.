import concurrent.futures
import json
import os
import pathlib
import sys
import time

cases = json.loads(pathlib.Path(__file__).with_name("season-4-cases.json").read_text(encoding="utf-8"))
assert len(cases) == len({case[1] for case in cases}) == 25
assert all(expected in choices and len(set(choices)) == 4 for _, _, _, choices, expected, _, _ in cases)

vercel = [
    "perplexity/pplx-decider-flash-v1.2", "baseten/mammoth-1", "baseten/elephant-1",
    "perplexity/pplx-decider-v1.2", "nace-ai/drex-max-v1.1",
]

def send(url, key, model, case):
    suite, name, task, options, expected, instructions, descriptions = case
    if url.endswith("/decisions"):
        payload = {"model": model, "input": task, "questions": [{
            "type": "choice", "name": "target", "instructions": instructions,
            "choices": [{"value": item, "description": descriptions.get(item, item)} for item in options],
        }]}
    else:
        payload = {"model": model, "state": task, "questions": {"target": {
            "type": "choice", "instructions": instructions,
            "criteria": {item: descriptions.get(item, item) for item in options},
        }}}
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError, URLError
    request = Request(url, data=json.dumps(payload).encode("utf-8"), headers={
        "Authorization": "Bearer " + key, "Content-Type": "application/json",
    })
    start = time.perf_counter()
    row = {"suite": suite, "case": name, "expected": expected}
    try:
        with urlopen(request, timeout=45) as response:
            data = json.load(response)
            status = response.status
    except HTTPError as exc:
        error = exc.read(8192).decode("utf-8", "replace")
        row.update(http=str(exc.code), curl_exit=1, wall_s=time.perf_counter()-start,
                   error=error.replace(key, "[redacted]")[:400])
        return row
    except (URLError, TimeoutError, ValueError) as exc:
        row.update(http="000", curl_exit=1, wall_s=time.perf_counter()-start,
                   error=str(exc).replace(key, "[redacted]")[:400])
        return row
    row.update(http=str(status), curl_exit=0, wall_s=time.perf_counter()-start)
    answers = data.get("answers", [])
    answer = next((x for x in answers if isinstance(x, dict) and x.get("name") == "target"), {}) if isinstance(answers, list) else answers.get("target", {})
    actual = answer.get("choice", answer.get("value", answer.get("answer"))) if isinstance(answer, dict) else answer
    if isinstance(actual, dict):
        actual = actual.get("value", actual.get("choice"))
    row.update(actual=actual, correct=actual == expected, served_model=data.get("model"), usage=data.get("usage"))
    if actual is None:
        row["answer_shape"] = answer
    return row


import benchmark_mercury as mercury
from urllib.error import HTTPError, URLError

jobs = [(model, "https://ai-gateway.vercel.sh/v1/decisions", "AI_GATEWAY_API_KEY", 13) for model in vercel]
jobs += [
    ("subq-snap-1.1", "https://api.subq.ai/v1/systemone", "SUBQ_API_KEY", 0),
    ("gpt-6-luna-decisions:free", "https://api.experientiallabs.ai/v1/decisions", "EXPERIENTIAL_API_KEY", 7),
    *[(model, "https://api.experientiallabs.ai/v1/systemone", "EXPERIENTIAL_API_KEY", 7)
      for model in ("jev-latest",)],
    ("mercury-decide", None, "INCEPTION_API_KEY", 0),
]
resume_rate_limited = len(sys.argv) == 3 and sys.argv[2] == "--resume-round-2-rate-limits"
if len(sys.argv) != 2 and not resume_rate_limited:
    raise SystemExit("Usage: benchmark_season4.py OUTPUT_DIRECTORY [--resume-round-2-rate-limits]")
output = pathlib.Path(sys.argv[1])
missing = sorted({job[2] for job in jobs if not os.environ.get(job[2])})
if missing:
    raise SystemExit("Missing environment variables: " + ", ".join(missing))
if not resume_rate_limited:
    output.mkdir(parents=True, exist_ok=False)


def result_path(folder, model):
    return folder / (model.replace("/", "__").replace(":", "_") + ".json")

def benchmark(job, folder):
    model, url, key_name, pace = job
    key = os.environ[key_name]
    path = result_path(folder, model)
    rows = json.loads(path.read_text(encoding="utf-8"))["rows"][:-1] if resume_rate_limited else []
    for index, case in enumerate(cases[len(rows):], start=len(rows)):
        started = time.perf_counter()
        suite, name, task, choices, expected, policy, _ = case
        if url:
            row = send(url, key, model, case)
            if resume_rate_limited and row["http"] == "429":
                print(model, name, "rate limited; waiting 65 s before retrying once", flush=True)
                time.sleep(65)
                row = send(url, key, model, case)
        else:
            try:
                actual, elapsed, usage, first = mercury.choose(key, model, task, choices, policy)
                row = dict(suite=suite, case=name, expected=expected, actual=actual,
                           correct=actual == expected, wall_s=elapsed, usage=usage,
                           first_text_s=first, http="200")
            except (HTTPError, URLError, TimeoutError, ValueError, KeyError) as exc:
                row = dict(suite=suite, case=name, expected=expected,
                           http=str(getattr(exc, "code", "000")), error=str(exc).replace(key, "[redacted]")[:300])
        rows.append(row)
        pending = path.with_suffix(".tmp")
        pending.write_text(json.dumps({"model": model, "rows": rows}, indent=2), encoding="utf-8")
        pending.replace(path)
        if row["http"] != "200" or row.get("actual") is None:
            print(model, name, row["http"], row.get("error", row.get("answer_shape")), flush=True)
            if row["http"] in {"401", "403", "404", "429"}:
                break
        if index < len(cases)-1:
            time.sleep(max(0, pace - (time.perf_counter()-started)))
    print(model, "answered", sum(r.get("actual") is not None for r in rows),
          "correct", sum(r.get("correct", False) for r in rows), flush=True)

selected = [
    "perplexity/pplx-decider-flash-v1.2", "mercury-decide",
    "baseten/mammoth-1", "baseten/elephant-1", "perplexity/pplx-decider-v1.2",
    "jev-latest", "nace-ai/drex-max-v1.1", "subq-snap-1.1", "gpt-6-luna-decisions:free",
]
job_by_id = {job[0]: job for job in jobs}
for round_number in ((2,) if resume_rate_limited else (1, 2)):
    folder = output / f"round-{round_number}"
    models = selected
    if resume_rate_limited:
        models = []
        for model in selected:
            path = result_path(folder, model)
            if not path.is_file():
                continue
            rows = json.loads(path.read_text(encoding="utf-8"))["rows"]
            if rows and rows[-1]["http"] == "429":
                models.append(model)
        if not models:
            raise SystemExit("No rate-limited Round 2 models to resume")
    else:
        folder.mkdir()
    print(f"Round {round_number} started", flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(models)) as pool:
        list(pool.map(lambda model: benchmark(job_by_id[model], folder), models))
    if round_number == 1:
        time.sleep(65)
    print(f"Round {round_number} finished", flush=True)
