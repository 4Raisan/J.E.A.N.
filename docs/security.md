# Security (Phase 6)

- Local-first: no listeners (named pipes only, DACL `D:P(A;;FA;;;<user SID>)`).
- Model never grants: policy engine has no model import; `--help` lists never-delegate set.
- Redaction: args/env/goal redacted; captured output NOT redacted (tool prints it).
- UIPI: integrity mismatch = typed blocker, never retry.
- Browser: never attach to operator profile; ProfileNotEmpty refused; endpoint loopback-checked.
- Runtime elevation is unsupported. The policy refuses elevated operations and the installer omits the refusal stub. Installer administrator consent is for setup.
- Licenses: cargo-deny green (GPL/tray/tauri/conpty/portable-pty/device_query banned; winapi via ntapi wrapper only).
- Weights: Zipformer Apache-2.0, Kokoro Apache-2.0, lexicon CMUdict BSD-2 w/ attribution.
