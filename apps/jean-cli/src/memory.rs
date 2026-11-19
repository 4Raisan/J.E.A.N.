//! Explicit, offline history and memory commands. No model or voice worker is started.
use clap::Subcommand;
use jean_core::{build_model, dream, memory, CancelFlag, Context};
use jean_store::{FactAddOutcome, MemoryNote, SavedFact, SearchOptions};
use std::process::ExitCode;

#[derive(Subcommand)]
#[command(after_help = "Example: jean history search \"build failed\" --limit 10")]
pub enum History {
    /// Search literal text; opt into SQLite FTS expressions with --fts-syntax.
    Search {
        query: String,
        #[arg(long)]
        fts_syntax: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Check the derived history index.
    Status,
    /// Rebuild the derived index without changing history records.
    Rebuild,
}
#[derive(Subcommand)]
#[command(
    after_help = "Examples:\n  jean memory add \"Use Rust\" --scope project-a\n  jean memory list --scope project-a\n  jean memory update ID \"Use stable Rust\"\n  jean memory forget ID\nNo scope selects global facts only; --scope includes global plus that project."
)]
pub enum Memory {
    Add {
        text: String,
        #[arg(long)]
        scope: Option<String>,
    },
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Search {
        query: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Get {
        id: String,
    },
    Update {
        id: String,
        text: String,
    },
    Forget {
        id: String,
    },
    /// Recall relevant saved facts, consolidated memory notes, playbooks, and episodes.
    Recall {
        query: String,
        #[arg(long)]
        scope: Option<String>,
        /// Print a single spoken-friendly summary sentence instead of tabular lines.
        #[arg(long)]
        spoken: bool,
    },
    /// Run a memory consolidation ("dream") cycle over recent episodes and tasks.
    Dream {
        /// Also invoke the configured main model for read-only deep synthesis.
        #[arg(long)]
        deep: bool,
        #[arg(long)]
        scope: Option<String>,
    },
    /// List self-updated and dreamed semantic memory notes.
    Notes {
        #[arg(long)]
        scope: Option<String>,
        /// Include superseded and archived notes.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// List recent episodic observations.
    Episodes {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List recent dream consolidation cycles.
    DreamLog {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Remove a consolidated/self-updated memory note by id.
    ForgetNote {
        id: String,
    },
    /// Record an observation and run self-updating preference/correction extraction.
    Observe {
        text: String,
        #[arg(long, default_value = "observation")]
        kind: String,
        #[arg(long)]
        scope: Option<String>,
    },
}
#[derive(Subcommand)]
#[command(
    after_help = "Examples:\n  jean preferences set voice.enabled false\n  jean preferences get voice.enabled\n  jean preferences clear voice.enabled\nSupported: voice.enabled, voice.mode, desktop.enabled, desktop.retention."
)]
pub enum Preferences {
    List,
    Get { key: String },
    Set { key: String, value: String },
    Clear { key: String },
}

/// Escape terminal controls (including ANSI ESC and bidi formatting) before printing.
/// The cap counts escaped output, so expansion cannot defeat it.
pub fn terminal(text: &str) -> String {
    let mut result = String::new();
    for c in text.chars() {
        let piece =
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_default().to_string()
            } else {
                c.to_string()
            };
        if result.len() + piece.len() > 2048 {
            result.push_str("...");
            break;
        }
        result.push_str(&piece);
    }
    result
}
fn fact(f: &SavedFact) {
    println!(
        "{}\tscope={}\tcreated={} updated={}\t{}",
        terminal(&f.id),
        terminal(if f.scope.is_empty() {
            "global"
        } else {
            &f.scope
        }),
        f.created_at_ms,
        f.updated_at_ms,
        terminal(&f.text)
    );
}
fn found(f: Option<SavedFact>) -> anyhow::Result<SavedFact> {
    f.ok_or_else(|| anyhow::anyhow!("fact not found"))
}
pub fn history(ctx: &mut Context, command: History) -> anyhow::Result<ExitCode> {
    match command {
        History::Search {
            query,
            fts_syntax,
            limit,
        } => {
            for h in memory::search_history(
                ctx,
                &SearchOptions {
                    query: &query,
                    fts_syntax,
                    limit: limit.min(100),
                },
            )? {
                println!(
                    "{}\t{}\t{}\tstate={} outcome={}\t{}",
                    h.source.as_str(),
                    terminal(&h.task_id),
                    h.at_ms,
                    terminal(h.state.as_deref().unwrap_or("unknown")),
                    terminal(h.outcome.as_deref().unwrap_or("none")),
                    terminal(&h.snippet)
                );
            }
        }
        History::Status => println!("{:?}", memory::history_status(ctx)?),
        History::Rebuild => println!("{:?}", memory::rebuild_history(ctx)?),
    }
    Ok(ExitCode::SUCCESS)
}
pub fn facts(ctx: &mut Context, command: Memory) -> anyhow::Result<ExitCode> {
    match command {
        Memory::Add { text, scope } => match memory::add_fact(ctx, &text, scope.as_deref())? {
            FactAddOutcome::Created(f) => {
                println!("created");
                fact(&f);
            }
            FactAddOutcome::Duplicate(f) => {
                println!("duplicate");
                fact(&f);
            }
        },
        Memory::List { scope, limit } => {
            for f in memory::list_facts(ctx, scope.as_deref(), limit.min(100))? {
                fact(&f);
            }
        }
        Memory::Search {
            query,
            scope,
            limit,
        } => {
            for hit in memory::search_facts(ctx, &query, scope.as_deref(), limit.min(100))? {
                fact(&hit.fact);
            }
        }
        Memory::Get { id } => fact(&found(memory::get_fact(ctx, &id)?)?),
        Memory::Update { id, text } => fact(&found(memory::update_fact(ctx, &id, &text)?)?),
        Memory::Forget { id } => {
            let f = found(memory::forget_fact(ctx, &id)?)?;
            println!("forgotten {}", terminal(&f.id));
        }
        Memory::Recall {
            query,
            scope,
            spoken,
        } => {
            let recalled = dream::recall_context(
                ctx,
                &query,
                scope.as_deref(),
                dream::DEFAULT_RECALL_CHARS,
            )?;
            if spoken {
                println!("{}", terminal(&recalled.render_spoken_summary()));
            } else if recalled.is_empty() {
                println!("No matching memories.");
            } else {
                for f in &recalled.facts {
                    fact(f);
                }
                for n in &recalled.notes {
                    print_note(n);
                }
                for pb in &recalled.playbooks {
                    println!(
                        "playbook\t{}\tverified={}\t{}",
                        terminal(&pb.name),
                        pb.verified_successes,
                        terminal(&pb.description)
                    );
                }
            }
        }
        Memory::Dream { deep, scope } => {
            let cancel = CancelFlag::new();
            let model = if deep && ctx.config.model.planning_enabled() {
                Some(build_model(&ctx.config.model)?)
            } else {
                None
            };
            let report = dream::dream(
                ctx,
                model.as_deref(),
                scope.as_deref(),
                cancel.atomic(),
            )?;
            println!("{}", terminal(&report.cycle.summary));
            for n in &report.updated_notes {
                print_note(n);
            }
            for pb in &report.promoted_playbooks {
                println!("promoted playbook\t{}", terminal(pb));
            }
        }
        Memory::Notes { scope, all, limit } => {
            let notes = ctx
                .store
                .list_memory_notes(scope.as_deref(), all, limit.min(100))?;
            if notes.is_empty() {
                println!("No consolidated memory notes yet.");
            } else {
                for n in &notes {
                    print_note(n);
                }
            }
        }
        Memory::Episodes { scope, limit } => {
            let eps = ctx
                .store
                .recent_episodes(scope.as_deref(), limit.min(100))?;
            if eps.is_empty() {
                println!("No episodes recorded yet.");
            } else {
                for ep in eps {
                    println!(
                        "{}\t[{}]\tconsolidated={}\tsalience={}\t{}",
                        terminal(&ep.id),
                        terminal(&ep.kind),
                        ep.consolidated,
                        ep.salience,
                        terminal(&ep.summary)
                    );
                }
            }
        }
        Memory::DreamLog { limit } => {
            let cycles = ctx.store.recent_dream_cycles(limit.min(50))?;
            if cycles.is_empty() {
                println!("No dream cycles recorded yet.");
            } else {
                for c in cycles {
                    println!(
                        "{}\tmode={}\tcreated={}\tsuperseded={}\tplaybooks={}\t{}",
                        terminal(&c.id),
                        terminal(&c.mode),
                        c.notes_created,
                        c.notes_superseded,
                        c.playbooks_promoted,
                        terminal(&c.summary)
                    );
                }
            }
        }
        Memory::ForgetNote { id } => {
            let removed = ctx
                .store
                .forget_memory_note(&id)?
                .ok_or_else(|| anyhow::anyhow!("memory note not found"))?;
            println!("forgotten note {}", terminal(&removed.id));
        }
        Memory::Observe { text, kind, scope } => {
            let updates = dream::extract_and_update_from_utterance(ctx, scope.as_deref(), &text)?;
            if updates.is_empty() {
                let _ = dream::observe_episode(
                    ctx,
                    &kind,
                    scope.as_deref(),
                    &text,
                    "manual",
                    60,
                    None,
                )?;
                println!("recorded episode");
            } else {
                for ev in updates {
                    println!("updated ({:?})", ev.kind);
                    print_note(&ev.note);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_note(n: &MemoryNote) {
    println!(
        "{}\t[{}]\tstatus={}\torigin={}\tsalience={}\trecalls={}\t{}",
        terminal(&n.id),
        terminal(&n.category),
        terminal(&n.status),
        terminal(&n.origin),
        n.salience,
        n.recall_count,
        terminal(&n.text)
    );
}
pub fn preferences(ctx: &mut Context, command: Preferences) -> anyhow::Result<ExitCode> {
    match command {
        Preferences::List => {
            for (key, meaning, values) in memory::supported_preferences() {
                println!("{key}: {meaning} [{}]", values.join(" | "));
            }
            for p in memory::list_preferences(ctx)?.into_iter().take(100) {
                println!("{} = {}", terminal(&p.key), terminal(&p.value));
            }
        }
        Preferences::Get { key } => {
            match ctx
                .store
                .preference_rows()?
                .into_iter()
                .find(|(k, _)| k == key.trim())
            {
                Some((key, value)) => println!("{} = {}", terminal(&key), terminal(&value)),
                None => println!("not stored (file/default applies)"),
            }
        }
        Preferences::Set { key, value } => {
            let p = memory::set_preference(ctx, &key, &value)?;
            println!("{} = {}", terminal(&p.key), terminal(&p.value));
        }
        Preferences::Clear { key } => println!(
            "{}",
            if memory::clear_preference(ctx, &key)? {
                "cleared"
            } else {
                "not stored"
            }
        ),
    }
    Ok(ExitCode::SUCCESS)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_controls_are_escaped_and_bounded() {
        let text = terminal("\x1b[2J\r\n\t\u{202e}");
        assert!(!text.chars().any(char::is_control));
        assert!(!text.contains('\u{202e}'));
        assert!(terminal(&"\x1b".repeat(5000)).len() <= 2051);
    }
}
