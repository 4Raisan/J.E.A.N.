//! Procedural memory ("Playbooks") for known and learned tasks.
//!
//! A playbook stores a validated, versioned recipe for a task JEAN has been taught
//! or has previously solved and independently verified. Replaying a playbook NEVER
//! bypasses policy, approvals, live target resolution, or verification: every step
//! resolves live on the current desktop/machine and passes through the same gates
//! as a freshly requested task.

use std::collections::BTreeMap;

use jean_store::{NewPlaybook, PlaybookRecord};
use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::dream::{new_id, short_topic_slug};
use crate::error::{CoreError, Result};

pub const MAX_PLAYBOOK_STEPS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybookParam {
    pub name: String,
    #[serde(default = "default_param_kind")]
    pub kind: String, // "text" | "url" | "path"
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_param_kind() -> String {
    "text".to_owned()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlaybookStep {
    RunCommand {
        tool: String,
        #[serde(default)]
        args: Vec<String>,
    },
    LaunchApp {
        app: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_true")]
        expect_window: bool,
    },
    InspectProcess {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        sort: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    LocateFile {
        root: String,
        pattern: String,
    },
    DesktopGoal {
        window: String,
        goal: String,
        #[serde(default)]
        launch: bool,
    },
    WorkspaceGoal {
        goal: String,
    },
    Browser {
        url: String,
        #[serde(default)]
        inspect: Option<String>,
        #[serde(default)]
        expect_non_empty: Option<String>,
        #[serde(default)]
        verify_video: bool,
        #[serde(default)]
        video_query: Option<String>,
    },
    OpenUrl {
        url: String,
    },
    Clock {
        scope: String, // "device" | "sri-lanka"
    },
    Gmail,
    Delegate {
        goal: String,
        #[serde(default)]
        worker: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchReason {
    ExactName,
    TriggerExact,
    TriggerTemplate,
    VerifiedRecall,
}

#[derive(Debug, Clone)]
pub struct MatchedPlaybook {
    pub record: PlaybookRecord,
    pub steps: Vec<PlaybookStep>,
    pub bound_params: BTreeMap<String, String>,
    pub reason: MatchReason,
    pub score: u32,
}

/// Validate and persist a typed playbook (creating version 1 or updating with previous_steps_json).
#[allow(clippy::too_many_arguments)]
pub fn save_typed_playbook(
    ctx: &Context,
    name: &str,
    display_name: &str,
    description: &str,
    triggers: &[String],
    scope: Option<&str>,
    parameters: &[PlaybookParam],
    steps: &[PlaybookStep],
    source: &str,
) -> Result<PlaybookRecord> {
    if steps.is_empty() || steps.len() > MAX_PLAYBOOK_STEPS {
        return Err(CoreError::Memory {
            reason: format!("a playbook must contain 1..={MAX_PLAYBOOK_STEPS} steps"),
        });
    }
    let clean_triggers: Vec<String> = triggers
        .iter()
        .map(|t| ctx.redactor.redact(t.trim()))
        .filter(|t| !t.is_empty())
        .take(12)
        .collect();
    let steps_json = serde_json::to_string(steps).map_err(|e| CoreError::Memory {
        reason: format!("could not serialize playbook steps: {e}"),
    })?;
    let triggers_json = serde_json::to_string(&clean_triggers).map_err(|e| CoreError::Memory {
        reason: format!("could not serialize playbook triggers: {e}"),
    })?;
    let parameters_json = serde_json::to_string(parameters).map_err(|e| CoreError::Memory {
        reason: format!("could not serialize playbook parameters: {e}"),
    })?;
    let pb = NewPlaybook {
        id: new_id("pb"),
        name: name.to_owned(),
        display_name: ctx.redactor.redact(display_name.trim()),
        description: ctx.redactor.redact(description.trim()),
        triggers_json,
        scope: scope.map(str::trim).unwrap_or("").to_owned(),
        parameters_json,
        steps_json,
        source: source.to_owned(),
    };
    Ok(ctx.store.save_playbook(&pb)?)
}

/// Parse stored steps from a `PlaybookRecord`.
pub fn parse_steps(record: &PlaybookRecord) -> Result<Vec<PlaybookStep>> {
    serde_json::from_str::<Vec<PlaybookStep>>(&record.steps_json).map_err(|e| CoreError::Memory {
        reason: format!("playbook {:?} has invalid steps: {e}", record.name),
    })
}

/// Parse stored trigger strings from a `PlaybookRecord`.
pub fn parse_triggers(record: &PlaybookRecord) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(&record.triggers_json).unwrap_or_default()
}

/// Parse stored parameters from a `PlaybookRecord`.
pub fn parse_params(record: &PlaybookRecord) -> Vec<PlaybookParam> {
    serde_json::from_str::<Vec<PlaybookParam>>(&record.parameters_json).unwrap_or_default()
}

/// Substitute `{param}` placeholders in playbook steps using validated bindings.
pub fn bind_steps(
    record: &PlaybookRecord,
    params: &BTreeMap<String, String>,
) -> Result<Vec<PlaybookStep>> {
    let declared = parse_params(record);
    for p in &declared {
        if p.required {
            let val = params.get(&p.name).map(|s| s.trim()).unwrap_or("");
            if val.is_empty() {
                return Err(CoreError::Memory {
                    reason: format!(
                        "playbook {:?} requires parameter {:?} (for example: with {} <value>)",
                        record.name, p.name, p.name
                    ),
                });
            }
            if p.kind == "url"
                && !val.starts_with("https://")
                && !val.starts_with("http://")
            {
                return Err(CoreError::Memory {
                    reason: format!("parameter {:?} requires an http:// or https:// URL", p.name),
                });
            }
        }
    }

    let raw_steps = parse_steps(record)?;
    let subst = |text: &str| -> String {
        let mut out = text.to_owned();
        for (k, v) in params {
            let token = format!("{{{k}}}");
            if out.contains(&token) {
                let replacement = if text.contains("youtube.com/results?search_query=")
                    || text.contains("bing.com/search?q=")
                {
                    url_encode_component(v)
                } else {
                    v.clone()
                };
                out = out.replace(&token, &replacement);
            }
        }
        out
    };

    let bound = raw_steps
        .into_iter()
        .map(|step| match step {
            PlaybookStep::RunCommand { tool, args } => PlaybookStep::RunCommand {
                tool,
                args: args.into_iter().map(|a| subst(&a)).collect(),
            },
            PlaybookStep::LaunchApp {
                app,
                args,
                expect_window,
            } => PlaybookStep::LaunchApp {
                app: subst(&app),
                args: args.into_iter().map(|a| subst(&a)).collect(),
                expect_window,
            },
            PlaybookStep::InspectProcess { name, sort, limit } => PlaybookStep::InspectProcess {
                name: name.map(|n| subst(&n)),
                sort,
                limit,
            },
            PlaybookStep::LocateFile { root, pattern } => PlaybookStep::LocateFile {
                root: subst(&root),
                pattern: subst(&pattern),
            },
            PlaybookStep::DesktopGoal {
                window,
                goal,
                launch,
            } => PlaybookStep::DesktopGoal {
                window: subst(&window),
                goal: subst(&goal),
                launch,
            },
            PlaybookStep::WorkspaceGoal { goal } => PlaybookStep::WorkspaceGoal {
                goal: subst(&goal),
            },
            PlaybookStep::Browser {
                url,
                inspect,
                expect_non_empty,
                verify_video,
                video_query,
            } => PlaybookStep::Browser {
                url: subst(&url),
                inspect: inspect.map(|i| subst(&i)),
                expect_non_empty: expect_non_empty.map(|s| subst(&s)),
                verify_video,
                video_query: video_query.map(|q| subst(&q)),
            },
            PlaybookStep::OpenUrl { url } => PlaybookStep::OpenUrl { url: subst(&url) },
            PlaybookStep::Clock { scope } => PlaybookStep::Clock { scope },
            PlaybookStep::Gmail => PlaybookStep::Gmail,
            PlaybookStep::Delegate { goal, worker } => PlaybookStep::Delegate {
                goal: subst(&goal),
                worker,
            },
        })
        .collect();
    Ok(bound)
}

/// Match a user utterance against JEAN's procedural memory (`playbooks`) to see
/// whether this is a **Known Task** JEAN already knows how to execute.
///
/// Precedence:
/// 1. Explicit invocation (`run playbook <name> [with <param> <val>]`).
/// 2. Exact trigger match (case-insensitive).
/// 3. Parameterized trigger template match (e.g. `"search {query} on youtube"`).
/// 4. High-confidence FTS5 recall over previously verified playbooks (`verified_successes >= 1`,
///    token Jaccard overlap >= 0.70, no unbound required parameters).
pub fn match_known_task(
    ctx: &Context,
    utterance: &str,
    scope: Option<&str>,
) -> Result<Option<MatchedPlaybook>> {
    let cleaned = normalize_utterance(utterance);
    if cleaned.is_empty() {
        return Ok(None);
    }

    // 1) Explicit "run playbook <name> [with <param> <value>]"
    if let Some((pb_name, params)) = parse_explicit_run(&cleaned) {
        if let Some(record) = ctx.store.get_playbook(&pb_name)? {
            let steps = bind_steps(&record, &params)?;
            return Ok(Some(MatchedPlaybook {
                record,
                steps,
                bound_params: params,
                reason: MatchReason::ExactName,
                score: 100,
            }));
        }
    }

    // 2) Exact and parameterized trigger matching across playbooks in scope.
    let all = ctx.store.list_playbooks(scope, 100)?;
    for record in &all {
        for trigger in parse_triggers(record) {
            let norm_trigger = normalize_utterance(&trigger);
            if norm_trigger.is_empty() {
                continue;
            }
            if norm_trigger == cleaned {
                let params = BTreeMap::new();
                if let Ok(steps) = bind_steps(record, &params) {
                    return Ok(Some(MatchedPlaybook {
                        record: record.clone(),
                        steps,
                        bound_params: params,
                        reason: MatchReason::TriggerExact,
                        score: 98,
                    }));
                }
            }
            if let Some(params) = match_trigger_template(&norm_trigger, &cleaned) {
                if let Ok(steps) = bind_steps(record, &params) {
                    return Ok(Some(MatchedPlaybook {
                        record: record.clone(),
                        steps,
                        bound_params: params,
                        reason: MatchReason::TriggerTemplate,
                        score: 94,
                    }));
                }
            }
        }
    }

    // 3) High-confidence FTS5 recall over verified playbooks (tasks JEAN has proven before).
    let candidates = ctx.store.search_playbooks(&cleaned, scope, 5)?;
    let query_tokens = word_set(&cleaned);
    if query_tokens.len() >= 2 {
        let mut best: Option<(u32, PlaybookRecord)> = None;
        for record in candidates {
            if record.verified_successes < 1 {
                continue;
            }
            if parse_params(&record).iter().any(|p| p.required) {
                continue;
            }
            let mut best_overlap = overlap_score(&query_tokens, &word_set(&record.description));
            for trigger in parse_triggers(&record) {
                best_overlap = best_overlap.max(overlap_score(&query_tokens, &word_set(&trigger)));
            }
            if best_overlap >= 75 {
                if best.as_ref().is_none_or(|(score, _)| best_overlap > *score) {
                    best = Some((best_overlap, record));
                }
            }
        }
        if let Some((score, record)) = best {
            let params = BTreeMap::new();
            if let Ok(steps) = bind_steps(&record, &params) {
                return Ok(Some(MatchedPlaybook {
                    record,
                    steps,
                    bound_params: params,
                    reason: MatchReason::VerifiedRecall,
                    score,
                }));
            }
        }
    }

    Ok(None)
}

/// Automatically capture or reinforce a playbook after JEAN solves a new task and
/// independently verifies success.
pub fn learn_from_verified_task(
    ctx: &Context,
    goal: &str,
    scope: Option<&str>,
    steps: &[PlaybookStep],
    task_id: Option<&str>,
    duration_ms: i64,
) -> Result<Option<PlaybookRecord>> {
    let trimmed = goal.trim();
    if trimmed.is_empty() || steps.is_empty() || steps.len() > MAX_PLAYBOOK_STEPS {
        return Ok(None);
    }
    if ctx.redactor.redact(trimmed) != trimmed {
        return Ok(None);
    }
    let name = short_topic_slug(trimmed);
    let existing = ctx.store.get_playbook(&name)?;
    let mut triggers = existing
        .as_ref()
        .map(parse_triggers)
        .unwrap_or_default();
    let norm = normalize_utterance(trimmed);
    if !triggers
        .iter()
        .any(|t| normalize_utterance(t) == norm)
    {
        triggers.push(trimmed.chars().take(180).collect());
    }
    let display: String = trimmed.chars().take(100).collect();
    let desc: String = format!("Learned from verified task: {trimmed}")
        .chars()
        .take(480)
        .collect();
    let saved = save_typed_playbook(
        ctx,
        &name,
        &display,
        &desc,
        &triggers,
        scope,
        &[],
        steps,
        "learned_from_task",
    )?;
    ctx.store.record_playbook_run(
        &saved.id,
        task_id,
        "verified_success",
        duration_ms,
        "captured after independent verification",
    )?;
    Ok(Some(saved))
}

fn normalize_utterance(text: &str) -> String {
    text.trim()
        .trim_end_matches(['.', '!', '?'])
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_explicit_run(cleaned: &str) -> Option<(String, BTreeMap<String, String>)> {
    let rest = cleaned
        .strip_prefix("run playbook ")
        .or_else(|| cleaned.strip_prefix("run workflow "))
        .or_else(|| cleaned.strip_prefix("/playbook run "))?;
    let mut params = BTreeMap::new();
    let (name_part, param_part) = if let Some((left, right)) = rest.split_once(" with ") {
        (left.trim(), Some(right.trim()))
    } else {
        (rest.trim(), None)
    };
    if name_part.is_empty() {
        return None;
    }
    if let Some(p) = param_part {
        if let Some((k, v)) = p.split_once(char::is_whitespace) {
            params.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let slug = name_part.split_whitespace().collect::<Vec<_>>().join("-");
    Some((slug, params))
}

/// Match a trigger containing a single `{param}` slot, e.g.
/// `"search {query} on youtube"` against `"search iron man trailer on youtube"`.
fn match_trigger_template(template: &str, utterance: &str) -> Option<BTreeMap<String, String>> {
    let open = template.find('{')?;
    let close = template[open + 1..].find('}')? + open + 1;
    let param_name = template[open + 1..close].trim();
    if param_name.is_empty() || template[close + 1..].contains('{') {
        return None;
    }
    let prefix = &template[..open];
    let suffix = &template[close + 1..];
    if !utterance.starts_with(prefix) || !utterance.ends_with(suffix) {
        return None;
    }
    if utterance.len() <= prefix.len() + suffix.len() {
        return None;
    }
    let captured = utterance[prefix.len()..utterance.len() - suffix.len()].trim();
    if captured.is_empty() {
        return None;
    }
    let mut map = BTreeMap::new();
    map.insert(param_name.to_owned(), captured.to_owned());
    Some(map)
}

fn word_set(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "to", "for", "my", "please", "me", "and", "of", "is", "are", "what",
        "in", "on", "it", "this", "that", "can", "you", "jean", "hey", "learned", "from",
        "verified", "task",
    ];
    let mut words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 2 && !STOP.contains(&w.as_str()))
        .collect();
    words.sort();
    words.dedup();
    words
}

fn overlap_score(query: &[String], candidate: &[String]) -> u32 {
    if query.is_empty() || candidate.is_empty() {
        return 0;
    }
    let common = query.iter().filter(|w| candidate.contains(w)).count();
    if common < 2 {
        return 0;
    }
    let union = query.len() + candidate.len() - common;
    let jaccard = if union == 0 { 0 } else { (common * 100) / union };
    let query_cov = (common * 100) / query.len();
    let cand_cov = (common * 100) / candidate.len();
    // If every meaningful word (or >= 85%) in a 3+ word query is contained in the learned
    // playbook trigger and covers at least half of it, recognize it as a high-confidence recall.
    let containment = if query.len() >= 3 && query_cov >= 85 && cand_cov >= 45 {
        (query_cov * 65 + cand_cov * 35) / 100
    } else {
        0
    };
    jaccard.max(containment) as u32
}

pub(crate) fn url_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ctx(tag: &str) -> (Context, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jean-playbooks-{tag}-{}-{}",
            std::process::id(),
            jean_store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Context::for_test(&dir);
        (ctx, dir)
    }

    #[test]
    fn known_task_matches_exact_template_and_verified_recall() {
        let (ctx, dir) = temp_ctx("match");

        save_typed_playbook(
            &ctx,
            "youtube-clips",
            "YouTube Clips",
            "Search YouTube for a video query",
            &["search {query} on youtube".into()],
            None,
            &[PlaybookParam {
                name: "query".into(),
                kind: "text".into(),
                required: true,
            }],
            &[PlaybookStep::OpenUrl {
                url: "https://www.youtube.com/results?search_query={query}".into(),
            }],
            "taught",
        )
        .unwrap();

        let matched = match_known_task(&ctx, "Search Iron Man suit up on YouTube", None)
            .unwrap()
            .expect("template trigger must match");
        assert_eq!(matched.reason, MatchReason::TriggerTemplate);
        assert_eq!(
            matched.bound_params.get("query").map(String::as_str),
            Some("iron man suit up")
        );
        assert_eq!(
            matched.steps,
            vec![PlaybookStep::OpenUrl {
                url: "https://www.youtube.com/results?search_query=iron+man+suit+up".into()
            }]
        );

        // Learn from a verified task and verify subsequent paraphrase recalls it.
        let learned = learn_from_verified_task(
            &ctx,
            "Check git status in repository",
            None,
            &[PlaybookStep::RunCommand {
                tool: "git.status".into(),
                args: vec![],
            }],
            Some("t-verify-1"),
            150,
        )
        .unwrap()
        .expect("playbook learned");
        assert_eq!(learned.source, "learned_from_task");

        let recalled = match_known_task(&ctx, "check the git status in my repository", None)
            .unwrap()
            .expect("verified playbook should be recalled");
        assert_eq!(recalled.reason, MatchReason::VerifiedRecall);
        assert_eq!(recalled.record.name, learned.name);

        // Concise follow-up recalls a longer learned trigger, while unrelated queries do not.
        let ram_pb = learn_from_verified_task(
            &ctx,
            "My computer feels slow, check what is using my RAM",
            None,
            &[PlaybookStep::InspectProcess {
                name: None,
                sort: Some("memory".into()),
                limit: Some(15),
            }],
            Some("t-verify-2"),
            90,
        )
        .unwrap()
        .expect("ram playbook learned");
        let ram_match = match_known_task(&ctx, "check what is using my RAM", None)
            .unwrap()
            .expect("concise follow-up should recall learned RAM playbook");
        assert_eq!(ram_match.record.name, ram_pb.name);
        assert!(
            match_known_task(&ctx, "check disk space on drive C", None)
                .unwrap()
                .is_none(),
            "unrelated query must not falsely match"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
