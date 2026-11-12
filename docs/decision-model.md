# Replacement-worker decision model

J.E.A.N. can ask a Chat Completions or System One endpoint for one decision during
checkpoint recovery. The choice is between two to eight already eligible
replacement workers. Rust first checks the checkpoint, disk space,
sign-in and network blockers, the verifier, and worker eligibility.
The endpoint receives numbered options, connector and sandbox types,
the previous connector type, and a typed recovery cause. Goals, prompts from the original task,
workspace paths, and worker names stay local.

The response is normalized to `{"choice": N}`, where a positive number names
one offered option and zero abstains. Rust retains the first eligible
worker on abstention, invalid output, timeout, rate limit, or exhausted
budget. Rust checks the workspace evidence again before reserving a
replacement. Worker approval and independent verification still apply.

## Observe first

The default is `mode = "off"`. Configure `[decision]` separately from the
planner and set `mode = "shadow"` to collect observations on actual
resumes. The Rust choice is used while shadowing. The bounded request
still runs during resume, so the configured deadline also bounds the
added provider wait. Set `base_url`, `model`, and `api_key_env` for the
endpoint. A remote endpoint requires verified HTTPS, the name of a
credential environment variable, and a positive per-call reservation.
For SoleAPI Jev, use `protocol = "system_one"` with
`base_url = "https://soleapi.com/v1"` and the exact model ID.

```text
jean decision inspect <original-task-id>
jean decision label <original-task-id> <expected-option-id>
jean decision report
```

Label a shadow observation with the option you judge correct using the
recorded choices and task evidence. The report compares labeled accuracy
with Rust, including the effect of fallback. The response must identify
the configured model exactly to count as valid. The report also shows invalid
answers, abstentions, HTTP 429s, incomplete calls, p95 latency, reported
tokens, and reserved cost.

`mode = "active"` permits this one decision only when the current
endpoint, model, and request contract have enough labeled valid shadow responses,
strictly better accuracy than Rust, zero invalid answers, at least 95%
valid responses, and p95 within `max_p95_ms`. Until then, active mode
collects shadow observations. Set `mode = "off"` to stop requests.

Before each request J.E.A.N. persists a reservation for the configured
`reserve_per_call_micros`. The sum and call count must fit
`budget_micros` and `max_calls` across the data directory. Reservations
survive failures, restarts, and provider changes. A remote per-call
reservation is the operator's conservative cost estimate, and the report
shows reservation amounts rather than provider invoices. The transport
requests at most 32 output tokens and makes one attempt per observation.
