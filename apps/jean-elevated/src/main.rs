//! Refusal stub for the reserved elevation helper.

use std::io::{self, Read};
use std::process::ExitCode;

use serde_json::{json, Value};

const MAX_REQUEST_BYTES: usize = 8192;

fn main() -> ExitCode {
    let mut input = Vec::new();
    let read = io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut input);
    let valid = read.is_ok()
        && input.len() <= MAX_REQUEST_BYTES
        && serde_json::from_slice::<Value>(&input)
            .ok()
            .and_then(|request| {
                request.as_object().map(|fields| {
                    fields.len() == 2
                        && ["operation", "target"].iter().all(|key| {
                            fields
                                .get(*key)
                                .and_then(Value::as_str)
                                .is_some_and(|value| !value.trim().is_empty())
                        })
                })
            })
            .unwrap_or(false);

    if !valid {
        println!("{}", json!({"ok": false, "reason": "malformed_request"}));
        return ExitCode::from(2);
    }

    println!(
        "{}",
        json!({"ok": false, "reason": "elevation_unsupported"})
    );
    ExitCode::from(5)
}
