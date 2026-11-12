# J.E.A.N.

J.E.A.N. is a Windows-focused Rust project for running and supervising local agent sessions. The workspace includes a CLI, a tray app, and shared crates for process supervision, policy, storage, and protocol integrations.

## Build and explore

Use Rust 1.90 or newer and the Visual Studio C++ build tools. Run these commands from the workspace root in an x64 Visual Studio developer shell.

```powershell
cargo build --workspace --locked
cargo test --workspace --locked
```

The project is in development. Start with the [implementation status](IMPLEMENTATION_STATUS.md) for tested behavior and work in progress. See the [implementation guide](IMPLEMENTATION_GUIDE.md) and [planning index](PLANNING/README.md) for architecture and roadmap details.

## Decision benchmarks

The [benchmark cases and runners](tools/decision-bench/) evaluate task routing and Windows UI decisions. The research snapshot dated **12 November 2026** used provider free tiers, welcome credits, promotional credits, and free trials. See [Decision Model Benchmarks](https://github.com/4Raisan/Decision-Model-Benchmarks) for the graph preview and results, or [download index.html](https://raw.githubusercontent.com/4Raisan/Decision-Model-Benchmarks/main/index.html) and open it locally.
