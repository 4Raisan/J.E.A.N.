//! Optional selection among Rust-eligible replacement workers.
//!
//! The model sees numbered options and typed recovery facts. Only the caller
//! can reserve or launch a worker, after reconciling its checkpoint.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use jean_protocol::config::{DecisionConfig, DecisionMode, WorkerConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::owner::RecoveryCause;
use crate::{ChatRequest, Context, CoreError, ModelError, Result};

pub(crate) const MAX_CHOICES: usize = 8;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
const MAX_RESERVATIONS: usize = 1000;
const PROMPT: &str = "Choose one eligible replacement worker for an interrupted task. +Return exactly one JSON object {\"choice\":N}. N must be a supplied numeric option ID, +or 0 to abstain. Options are ordered by the operator's standing preference. +Abstain unless the supplied connector and sandbox facts justify a choice. +You cannot authorize, execute, or invent an option.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    choice: usize,
}

fn parse_answer(content: &str, choice_count: usize) -> (&'static str, Option<usize>) {
    if !content.trim_start().starts_with('{') {
        return ("invalid_response", None);
    }
    match serde_json::from_str::<Answer>(content) {
        Ok(Answer { choice: 0 }) => ("abstained", None),
        Ok(Answer { choice }) if choice <= choice_count => ("selected", Some(choice)),
        Ok(_) => ("invalid_choice", None),
        Err(_) => ("invalid_response", None),
    }
}

#[derive(Serialize, Deserialize)]
struct Choice {
    name: String,
    connector: String,
    sandbox: String,
}

#[derive(Serialize, Deserialize)]
struct Reservation {
    task_id: String,
    cohort: String,
    shadow: bool,
    cause: RecoveryCause,
    previous_connector: String,
    choices: Vec<Choice>,
    reserved_micros: u64,
    baseline_us: u64,
}

#[derive(Serialize, Deserialize)]
struct Outcome {
    status: String,
    suggestion: Option<usize>,
    elapsed_ms: u64,
    reported_tokens: Option<u64>,
    applied_choice: usize,
}

#[derive(Serialize, Default)]
pub struct Report {
    pub reserved_calls: usize,
    pub reserved_micros: u64,
    pub shadow_calls: usize,
    pub active_calls: usize,
    pub labeled: usize,
    pub baseline_correct: usize,
    pub selector_with_fallback_correct: usize,
    pub valid_responses: usize,
    pub abstentions: usize,
    pub invalid_responses: usize,
    pub rate_limited: usize,
    pub failed_or_incomplete: usize,
    pub p95_ms: Option<u64>,
    pub baseline_p95_us: Option<u64>,
    pub reported_tokens: u64,
    pub qualified: bool,
}

fn error(reason: impl Into<String>) -> CoreError {
    CoreError::Intent {
        reason: reason.into(),
    }
}

fn io(error_value: std::io::Error) -> CoreError {
    error(format!("decision ledger: {error_value}"))
}

fn root(ctx: &Context) -> PathBuf {
    ctx.paths.data_dir.join("decision-model")
}

fn lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir).map_err(io)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("ledger.lock"))
        .map_err(io)?;
    lock.try_lock()
        .map_err(|e| error(format!("decision ledger is busy: {e}")))?;
    Ok(lock)
}

fn path(dir: &Path, id: &str, suffix: &str) -> PathBuf {
    dir.join(format!("{id}.{suffix}.json"))
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(error("decision record exceeds its size limit"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| error(format!("invalid decision record: {e}")))
}

fn write(path: &Path, record: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|e| error(e.to_string()))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(error("decision record exceeds its size limit"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(io)
}

fn cohort(config: &DecisionConfig) -> String {
    let contract = serde_json::json!([
        "replacement-worker-v2",
        PROMPT,
        config.base_url,
        config.protocol,
        config.model,
        config.api_key_env,
        config.timeout_secs,
        config.transport().max_output_tokens,
    ]);
    Sha256::digest(contract.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reservations(dir: &Path) -> Result<Vec<Reservation>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(e)),
    };
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io)?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".call.json"))
        else {
            continue;
        };
        if result.len() == MAX_RESERVATIONS {
            return Err(error("decision ledger exceeds its reservation limit"));
        }
        jean_protocol::ipc::validate_task_id(id)?;
        let record: Reservation =
            read(&entry.path())?.ok_or_else(|| error("reservation disappeared"))?;
        if record.task_id != id || !(2..=MAX_CHOICES).contains(&record.choices.len()) {
            return Err(error("invalid decision reservation identity or choices"));
        }
        result.push(record);
    }
    Ok(result)
}

fn p95(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() * 95).div_ceil(100) - 1])
}

fn summarize(dir: &Path, config: &DecisionConfig, calls: &[Reservation]) -> Result<Report> {
    let cohort = cohort(config);
    let mut report = Report {
        reserved_calls: calls.len(),
        ..Report::default()
    };
    let mut latencies = Vec::new();
    let mut baselines = Vec::new();
    let mut labeled_valid = 0;
    for call in calls {
        report.reserved_micros = report
            .reserved_micros
            .checked_add(call.reserved_micros)
            .ok_or_else(|| error("decision budget accounting overflow"))?;
        if call.cohort != cohort {
            continue;
        }
        if !call.shadow {
            report.active_calls += 1;
            continue;
        }
        report.shadow_calls += 1;
        baselines.push(call.baseline_us);
        let outcome: Option<Outcome> = read(&path(dir, &call.task_id, "result"))?;
        let valid_before = report.valid_responses;
        let suggested = if let Some(outcome) = outcome {
            latencies.push(outcome.elapsed_ms);
            report.reported_tokens = report
                .reported_tokens
                .saturating_add(outcome.reported_tokens.unwrap_or(0));
            match outcome.status.as_str() {
                "selected"
                    if outcome
                        .suggestion
                        .is_some_and(|id| (1..=call.choices.len()).contains(&id)) =>
                {
                    report.valid_responses += 1;
                    outcome.suggestion
                }
                "abstained" => {
                    report.valid_responses += 1;
                    report.abstentions += 1;
                    None
                }
                "invalid_choice" | "invalid_response" | "model_mismatch" | "selected" => {
                    report.invalid_responses += 1;
                    None
                }
                "rate_limited" => {
                    report.rate_limited += 1;
                    None
                }
                _ => {
                    report.failed_or_incomplete += 1;
                    None
                }
            }
        } else {
            report.failed_or_incomplete += 1;
            None
        };
        if let Some(expected) = read::<usize>(&path(dir, &call.task_id, "label"))? {
            if !(1..=call.choices.len()).contains(&expected) {
                return Err(error("decision label is outside the recorded option list"));
            }
            report.labeled += 1;
            labeled_valid += (report.valid_responses > valid_before) as usize;
            report.baseline_correct += (expected == 1) as usize;
            report.selector_with_fallback_correct += (expected == suggested.unwrap_or(1)) as usize;
        }
    }
    report.p95_ms = p95(&mut latencies);
    report.baseline_p95_us = p95(&mut baselines);
    report.qualified = labeled_valid >= config.min_labeled
        && report.selector_with_fallback_correct > report.baseline_correct
        && report.invalid_responses == 0
        && report.valid_responses * 100 >= report.shadow_calls * 95
        && report.p95_ms.is_some_and(|ms| ms <= config.max_p95_ms);
    Ok(report)
}

pub fn report(ctx: &Context) -> Result<Report> {
    let dir = root(ctx);
    let _lock = if dir.try_exists().map_err(io)? {
        Some(lock(&dir)?)
    } else {
        None
    };
    summarize(&dir, &ctx.config.decision, &reservations(&dir)?)
}

pub fn inspect(ctx: &Context, id: &str) -> Result<serde_json::Value> {
    jean_protocol::ipc::validate_task_id(id)?;
    let dir = root(ctx);
    if !dir.is_dir() {
        return Err(error("no selector observation for this task"));
    }
    let _lock = lock(&dir)?;
    let call: Reservation = read(&path(&dir, id, "call"))?
        .ok_or_else(|| error("no selector observation for this task"))?;
    let outcome: Option<Outcome> = read(&path(&dir, id, "result"))?;
    let label: Option<usize> = read(&path(&dir, id, "label"))?;
    Ok(serde_json::json!({
        "task_id": call.task_id, "shadow": call.shadow, "cause": call.cause,
        "previous_connector": call.previous_connector,
        "baseline_choice": 1,
        "choices": call.choices.iter().enumerate().map(|(i, worker)| serde_json::json!({
            "id": i + 1, "worker": worker.name, "connector": worker.connector, "sandbox": worker.sandbox,
        })).collect::<Vec<_>>(),
        "outcome": outcome, "expected_choice": label,
    }))
}

pub fn label(ctx: &Context, id: &str, choice: usize) -> Result<()> {
    jean_protocol::ipc::validate_task_id(id)?;
    let dir = root(ctx);
    let _lock = lock(&dir)?;
    let call: Reservation = read(&path(&dir, id, "call"))?
        .ok_or_else(|| error("no selector observation for this task"))?;
    if !call.shadow || !(1..=call.choices.len()).contains(&choice) {
        return Err(error(
            "label must name an eligible choice from a shadow observation",
        ));
    }
    let file = path(&dir, id, "label");
    match read::<usize>(&file)? {
        Some(previous) if previous == choice => Ok(()),
        Some(_) => Err(error(
            "this shadow observation already has a different label",
        )),
        None => write(&file, &choice),
    }
}

/// Returns a zero-based index into the current eligible workers. Zero is always
/// the deterministic fallback. The caller owns cancellation and eligibility.
pub(crate) fn choose(
    ctx: &Context,
    id: &str,
    cause: RecoveryCause,
    previous_connector: &str,
    workers: &[&WorkerConfig],
    baseline_us: u64,
    cancel: &AtomicBool,
) -> usize {
    if ctx.config.decision.mode == DecisionMode::Off
        || !(2..=MAX_CHOICES).contains(&workers.len())
        || cancel.load(Ordering::Relaxed)
    {
        return 0;
    }
    match choose_recorded(
        ctx,
        id,
        cause,
        previous_connector,
        workers,
        baseline_us,
        cancel,
    ) {
        Ok(index) => index,
        Err(_) => {
            let _ = ctx.store.append_event(
                id,
                &jean_protocol::Event::note(
                    "decision selector unavailable or budget exhausted; using the Rust fallback",
                ),
            );
            0
        }
    }
}

fn choose_recorded(
    ctx: &Context,
    id: &str,
    cause: RecoveryCause,
    previous_connector: &str,
    workers: &[&WorkerConfig],
    baseline_us: u64,
    cancel: &AtomicBool,
) -> Result<usize> {
    let config = &ctx.config.decision;
    let previous_connector = match previous_connector {
        "acp" => "acp",
        "codex-cli" => "codex-cli",
        _ => "unknown",
    };
    let client = jean_model::build(&config.transport())?;
    let dir = root(ctx);
    let active = {
        let _lock = lock(&dir)?;
        let calls = reservations(&dir)?;
        let report = summarize(&dir, config, &calls)?;
        if calls.len() >= config.max_calls as usize
            || calls.iter().any(|record| record.task_id == id)
            || report
                .reserved_micros
                .checked_add(config.reserve_per_call_micros)
                .is_none_or(|sum| sum > config.budget_micros)
        {
            return Err(error("decision request reservation unavailable"));
        }
        let active = config.mode == DecisionMode::Active && report.qualified;
        let call = Reservation {
            task_id: id.into(),
            cohort: cohort(config),
            shadow: !active,
            cause,
            previous_connector: previous_connector.into(),
            choices: workers
                .iter()
                .map(|worker| Choice {
                    name: worker.name.clone(),
                    connector: worker.kind.clone(),
                    sandbox: worker.sandbox.clone(),
                })
                .collect(),
            reserved_micros: config.reserve_per_call_micros,
            baseline_us,
        };
        write(&path(&dir, id, "call"), &call)?;
        active
    };
    let request = ChatRequest::new(
        PROMPT,
        serde_json::json!({
            "recovery_cause": cause,
            "previous_connector": previous_connector,
            "options": workers.iter().enumerate().map(|(index, worker)| serde_json::json!({
                "id": index + 1, "connector": worker.kind, "sandbox": worker.sandbox,
            })).collect::<Vec<_>>(),
        })
        .to_string(),
    );
    let start = Instant::now();
    let mut outcome = Outcome {
        status: "invalid_response".into(),
        suggestion: None,
        elapsed_ms: 0,
        reported_tokens: None,
        applied_choice: 1,
    };
    let reply = if config.protocol == "system_one" {
        system_one(config, &request, workers, cancel)
    } else {
        client.generate(&request, cancel)
    };
    match reply {
        Ok(reply) => {
            outcome.reported_tokens = reply.tokens_used;
            if reply.model.as_deref() != Some(config.model.as_str()) {
                outcome.status = "model_mismatch".into();
            } else if !reply.truncated() {
                let (status, choice) = parse_answer(&reply.content, workers.len());
                outcome.status = status.into();
                outcome.suggestion = choice;
                if active && !cancel.load(Ordering::Relaxed) {
                    outcome.applied_choice = choice.unwrap_or(1);
                }
            }
        }
        Err(ModelError::HttpStatus { status: 429, .. }) => outcome.status = "rate_limited".into(),
        Err(error) => outcome.status = error.code().into(),
    }
    outcome.elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    {
        let _lock = lock(&dir)?;
        write(&path(&dir, id, "result"), &outcome)?;
    }
    let _ = ctx.store.append_event(
        id,
        &jean_protocol::Event::note(format!(
            "decision selector {}: {}; Rust eligible option {} selected",
            if active { "active" } else { "shadow" },
            outcome.status,
            outcome.applied_choice,
        )),
    );
    Ok(outcome.applied_choice - 1)
}

fn system_one(
    config: &DecisionConfig,
    request: &ChatRequest,
    workers: &[&WorkerConfig],
    cancel: &AtomicBool,
) -> jean_model::Result<jean_model::ChatReply> {
    use jean_model::http::{OutgoingRequest, Target};
    use std::time::Duration;

    let target = Target::parse(&config.base_url)?;
    let path = target.join("/systemone");
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    if !config.api_key_env.is_empty() {
        let secret = jean_model::Secret::from_env(&config.api_key_env);
        let key = secret
            .reveal()
            .ok_or_else(|| ModelError::MissingCredential {
                name: config.api_key_env.clone(),
            })?;
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    let mut criteria = serde_json::Map::new();
    criteria.insert(
        "0".into(),
        "Abstain; let the operator's standing preference decide.".into(),
    );
    for (index, worker) in workers.iter().enumerate() {
        criteria.insert(
            (index + 1).to_string(),
            format!(
                "Preference rank {}. Connector {}, sandbox {}.",
                index + 1,
                worker.kind,
                worker.sandbox
            )
            .into(),
        );
    }
    let outgoing = OutgoingRequest {
        method: "POST",
        path,
        headers,
        body: serde_json::json!({
            "model": config.model, "state": request.user,
            "questions": {"target": {"type": "choice",
                "instructions": "Choose an offered replacement worker only when the connector and sandbox facts justify it; otherwise select 0.",
                "criteria": criteria}},
        }).to_string().into_bytes(),
        timeout: Duration::from_secs(config.timeout_secs),
        max_response_bytes: config.transport().max_response_bytes,
    };
    let started = Instant::now();
    let response = jean_model::http::send(&target, &outgoing, cancel)?;
    if response.status != 200 {
        return Err(ModelError::HttpStatus {
            status: response.status,
            endpoint: format!("{}{}", target.display(), outgoing.path),
            detail: "System One rejected the request".into(),
        });
    }
    let shape = || ModelError::ResponseShape {
        provider: "system_one",
        detail: "expected a numbered target choice".into(),
    };
    let value: serde_json::Value =
        serde_json::from_str(&response.body_text()).map_err(|_| shape())?;
    let choice = value["answers"]["target"]["choice"]
        .as_str()
        .and_then(|choice| choice.parse::<usize>().ok())
        .ok_or_else(shape)?;
    let usage = &value["usage"];
    let tokens_used = usage["input_tokens"]
        .as_u64()
        .zip(usage["output_tokens"].as_u64())
        .map(|(input, output)| input.saturating_add(output));
    Ok(jean_model::ChatReply {
        content: serde_json::json!({"choice": choice}).to_string(),
        model: value["model"].as_str().map(str::to_owned),
        finish_reason: None,
        tokens_used,
        elapsed_ms: started.elapsed().as_millis() as u64,
        attempts: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_contract_accepts_only_offered_ids_and_abstention() {
        assert_eq!(parse_answer(r#"{"choice":2}"#, 2), ("selected", Some(2)));
        assert_eq!(parse_answer(r#"{"choice":0}"#, 2), ("abstained", None));
        assert_eq!(parse_answer(r#"{"choice":3}"#, 2), ("invalid_choice", None));
        for invalid in [
            r#"{"choice":1,"command":"run"}"#,
            r#"{"choice":1,"choice":2}"#,
            r#"{"choice":"1"}"#,
            "[1]",
            r#"{"choice":-1}"#,
            r#"{"choice":1} trailing"#,
        ] {
            assert_eq!(parse_answer(invalid, 2), ("invalid_response", None));
        }
    }
}
