//! Narrow elevation helper. One op per invocation, launched via UAC (`runas`).
//! stdin: {"operation": "...", "target": "..."} — exactly what JEAN approved.
//! stdout: JSON result. No shell, no password storage, no retries.
//! Pipe ACL + integrity check live in the caller (jean-core policy layer).

use anyhow::Result;

fn main() -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let n = std::io::Read::read(&mut std::io::stdin(), &mut buf)?;
    let req: serde_json::Value = serde_json::from_slice(&buf[..n])?;
    let op = req.get("operation").and_then(|v| v.as_str()).unwrap_or("");
    // Stub: elevated surface is intentionally tiny. Real ops are added per
    // runbook; an unknown op is a refusal, not a guess.
    let out = match op {
        "format_volume" => serde_json::json!({"ok": false, "reason": "auto-deny, never elevated"}),
        _ => serde_json::json!({"ok": false, "reason": format!("unknown operation: {op}")}),
    };
    println!("{}", out);
    Ok(())
}
