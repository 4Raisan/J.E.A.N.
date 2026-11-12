//! Fresh-process recovery through `jean goal` and `jean resume`.
//! The worker is a local ACP fixture; quota is recognized only from structured error data.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Option<Self> {
        let node = jean_core::resolve::resolve_executable(
            "node",
            std::env::var("PATH").ok().as_deref(),
            std::env::var("PATHEXT").ok().as_deref(),
        )
        .ok()?;
        let git = jean_core::resolve::resolve_executable(
            "git",
            std::env::var("PATH").ok().as_deref(),
            std::env::var("PATHEXT").ok().as_deref(),
        )
        .ok()?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jean-owner-resume-{tag}-{}-{nonce}",
            std::process::id()
        ));
        let data = root.join("data");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&data).ok()?;
        std::fs::create_dir_all(&workspace).ok()?;
        std::fs::write(workspace.join("base.txt"), "base\n").ok()?;
        if tag.starts_with("profile") {
            std::fs::write(workspace.join("stage-one.txt"), "first effect\n").ok()?;
            std::fs::write(workspace.join("stage-two.txt"), "second effect\n").ok()?;
        }
        let git_run = |args: &[&str]| {
            Command::new(&git)
                .args(args)
                .current_dir(&workspace)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        if !git_run(&["init", "-q"])
            || !git_run(&["add", "."])
            || !git_run(&[
                "-c",
                "user.email=fixture@jean",
                "-c",
                "user.name=fixture",
                "commit",
                "-qm",
                "base",
            ])
        {
            let _ = std::fs::remove_dir_all(&root);
            return None;
        }

        let alpha = root.join("alpha.js");
        let beta = root.join("beta.js");
        let marker = root.join("beta-started.txt");
        let verify_marker = root.join("verify-runs.txt");
        let failure_kind = match tag {
            "context" => "context_exhausted",
            "sign-in" => "sign_in_expired",
            "network" => "network_unavailable",
            "decision-unknown" => "unexpected_server_error",
            "review" => "review",
            "profile-fail" => "profile-fail",
            "profile-quick" | "profile-polished" | "profile-override" => "profile-worker",
            _ => "quota_exhausted",
        };
        std::fs::write(&alpha, ALPHA_SCRIPT).ok()?;
        std::fs::write(&beta, BETA_SCRIPT).ok()?;
        let verification = if tag == "review" {
            r"const fs=require('fs');process.exit(fs.readFileSync('stage-one.txt','utf8')==='first effect\n'?0:1)".to_owned()
        } else if tag.starts_with("profile") {
            format!(
                "const fs=require('fs');const p={};fs.appendFileSync(p,'run\\n');const count=fs.readFileSync(p,'utf8').trim().split('\\n').length;process.exit(fs.readFileSync({},'utf8')==='first effect\\n'&&fs.readFileSync({},'utf8')==='second effect\\n'&&{}?0:1)",
                js_string(&verify_marker),
                js_string(&workspace.join("stage-one.txt")),
                js_string(&workspace.join("stage-two.txt")),
                if tag == "profile-fail" {
                    "count===1"
                } else {
                    "true"
                },
            )
        } else {
            format!(
                "const fs=require('fs');process.exit(fs.readFileSync({},'utf8')==='first effect\\n'&&fs.readFileSync({},'utf8')==='second effect\\n'?0:1)",
                js_string(&workspace.join("stage-one.txt")),
                js_string(&workspace.join("stage-two.txt")),
            )
        };
        let mut config = format!(
            "[model]\nprovider='none'\n\n[workspace]\nroot={}\n\n[delegation]\nenabled=true\ndefault_worker='alpha'\nverify_command={}\nverify_args=['-e',{}]\nverify_timeout_secs=30\nsession_timeout_secs=30\nidle_timeout_secs=10\npermission_timeout_secs=10\nshutdown_grace_ms=300\nmax_event_queue=64\nmax_output_bytes=262144\nresume_min_free_disk_bytes=1\nisolate_in_worktree=false\nrequire_clean_baseline=false\n\n[[delegation.workers]]\nname='alpha'\nkind='acp'\ncommand={}\nargs=[{},{},{}]\ncapability='agent.delegate'\nsandbox='workspace-write'\nenabled=true\nfree_fallback=false\n\n[[delegation.workers]]\nname='beta'\nkind='acp'\nmodel='fixture/beta'\ncommand={}\nargs=[{},{}]\ncapability='agent.delegate'\nsandbox='workspace-write'\nenabled=true\nfree_fallback=true\n",
            toml_string(&workspace.display().to_string()),
            toml_string(&node.display().to_string()),
            toml_string(&verification),
            toml_string(&node.display().to_string()),
            toml_string(&alpha.display().to_string()),
            toml_string(failure_kind),
            toml_string(&marker.display().to_string()),
            toml_string(&node.display().to_string()),
            toml_string(&beta.display().to_string()),
            toml_string(&marker.display().to_string()),
        );
        if tag == "decision-no-fallback" {
            config = config.replace("free_fallback=true", "free_fallback=false");
        }
        if tag == "review" {
            config = config.replace("isolate_in_worktree=false", "isolate_in_worktree=true");
            config.push_str("\n[policy]\nrequire_approval=[]\n");
        }
        if tag == "profile-quick" {
            let checks = |profile: &str| {
                let script = format!(
                    "require('fs').appendFileSync({},'{}\\n');{verification}",
                    js_string(&verify_marker),
                    profile,
                );
                format!("['-e',{}]", toml_string(&script))
            };
            config = config.replacen(
                "verify_timeout_secs=30",
                &format!(
                    "verify_quick_prototype_args={}\nverify_polished_ui_args={}\nverify_timeout_secs=30",
                    checks("quick"),
                    checks("polished")
                ),
                1,
            );
        }
        std::fs::write(data.join("jean.toml"), config).ok()?;
        Some(Self {
            root,
            data,
            workspace,
        })
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_jean"))
            .args(args)
            .env("JEAN_DATA_DIR", &self.data)
            .output()
            .expect("launch a fresh JEAN process")
    }

    fn start_recoverable_task(&self) -> String {
        let task_id = self.start_task();
        let checkpoint = self.checkpoint(&task_id);
        assert_eq!(checkpoint["resumable"], true);
        task_id
    }

    fn start_task(&self) -> String {
        let workspace = self.workspace.display().to_string();
        let output = self.run(&[
            "goal",
            "Create stage one, then stage two with the exact requested contents",
            "--workspace",
            &workspace,
            "--worker",
            "alpha",
        ]);
        assert_ne!(
            output.status.code(),
            Some(0),
            "fixture should stop before completion"
        );
        assert!(
            output_text(&output).contains("baseline check: did not pass"),
            "the configured independent check must fail before the worker edit: {}",
            output_text(&output)
        );
        assert_eq!(
            std::fs::read_to_string(self.workspace.join("stage-one.txt")).unwrap(),
            "first effect\n"
        );
        assert!(!self.workspace.join("stage-two.txt").exists());
        let task_id = output_text(&output)
            .lines()
            .find_map(|line| line.strip_prefix("task: ").map(str::to_owned))
            .or_else(|| {
                std::fs::read_dir(self.data.join("checkpoints"))
                    .ok()?
                    .filter_map(Result::ok)
                    .find(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .and_then(|entry| {
                        serde_json::from_slice::<serde_json::Value>(
                            &std::fs::read(entry.path()).ok()?,
                        )
                        .ok()?
                        .get("task_id")?
                        .as_str()
                        .map(str::to_owned)
                    })
            })
            .expect("goal checkpoint or task output exposes its task id");
        assert!(
            output_text(&output)
                .lines()
                .any(|line| line == format!("task: {task_id}")),
            "goal output should expose the resumable task id: {}",
            output_text(&output)
        );
        task_id
    }

    fn checkpoint(&self, task_id: &str) -> serde_json::Value {
        let path = self
            .data
            .join("checkpoints")
            .join(format!("{task_id}.json"));
        serde_json::from_slice(&std::fs::read(path).expect("checkpoint file exists")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn baseline_case(name: &str, expected: &str, observed: &str, elapsed: std::time::Duration) {
    let matched = expected == observed;
    println!(
        "decision_baseline case={name} expected={expected} observed={observed} matched={matched} resume_wall_us={}",
        elapsed.as_micros()
    );
    assert!(matched, "routing baseline case {name} disagreed");
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn js_string(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string().replace('\\', "/")).unwrap()
}

const ALPHA_SCRIPT: &str = r#"
const fs=require('fs');const readline=require('readline');const rl=readline.createInterface({input:process.stdin});
const send=o=>process.stdout.write(JSON.stringify(o)+'\n');let promptId;const failureKind=process.argv[2]||'quota_exhausted';const marker=process.argv[3];
rl.on('line',line=>{let m;try{m=JSON.parse(line)}catch{return}
if(m.method==='initialize')send({jsonrpc:'2.0',id:m.id,result:{protocolVersion:1,agentCapabilities:{},agentInfo:{name:'quota-fixture',version:'1'}}});
else if(m.method==='session/new')send({jsonrpc:'2.0',id:m.id,result:{sessionId:'alpha-session'}});
else if(m.method==='session/prompt'){promptId=m.id;if(failureKind==='profile-worker'||failureKind==='profile-fail'){fs.appendFileSync(marker,JSON.stringify(m.params)+'\n');send({jsonrpc:'2.0',id:promptId,result:{stopReason:'end_turn'}})}else{send({jsonrpc:'2.0',id:'alpha-write',method:'fs/write_text_file',params:{sessionId:'alpha-session',path:process.cwd().replace(/\\/g,'/')+'/stage-one.txt',content:'first effect\n'}})}}
else if(m.id==='alpha-write'){if(failureKind==='review'){send({jsonrpc:'2.0',id:promptId,result:{stopReason:'end_turn'}})}else{send({jsonrpc:'2.0',id:promptId,error:{code:-32001,message:'structured recovery fixture',data:{kind:failureKind,remaining_work:'Create stage-two.txt containing exactly second effect followed by a newline.'}}})}}
});
"#;

const BETA_SCRIPT: &str = r#"
const fs=require('fs');const readline=require('readline');const rl=readline.createInterface({input:process.stdin});
const send=o=>process.stdout.write(JSON.stringify(o)+'\n');let promptId;const marker=process.argv[2];
fs.writeFileSync(marker,'started\n');
rl.on('line',line=>{let m;try{m=JSON.parse(line)}catch{return}
if(m.method==='initialize')send({jsonrpc:'2.0',id:m.id,result:{protocolVersion:1,agentCapabilities:{},agentInfo:{name:'free-fixture',version:'1'}}});
else if(m.method==='session/new')send({jsonrpc:'2.0',id:m.id,result:{sessionId:'beta-session',configOptions:[{id:'model',name:'Model',category:'model',type:'select',currentValue:'fixture/beta',options:[{value:'fixture/beta',name:'Beta'}]}]}});
else if(m.method==='session/set_config_option'&&m.params.value==='fixture/beta')send({jsonrpc:'2.0',id:m.id,result:{configOptions:[{id:'model',name:'Model',category:'model',type:'select',currentValue:'fixture/beta',options:[{value:'fixture/beta',name:'Beta'}]}]}});
else if(m.method==='session/prompt'){promptId=m.id;send({jsonrpc:'2.0',method:'session/update',params:{sessionId:'beta-session',update:{sessionUpdate:'agent_message_chunk',content:{type:'text',text:'Writing the remaining file'}}}});fs.appendFileSync(marker,JSON.stringify(m.params)+'\n');send({jsonrpc:'2.0',id:'beta-write',method:'fs/write_text_file',params:{sessionId:'beta-session',path:process.cwd().replace(/\\/g,'/')+'/stage-two.txt',content:'second effect\n'}})}
else if(m.id==='beta-write')send({jsonrpc:'2.0',id:promptId,result:{stopReason:'end_turn'}});
});
"#;

const BETA_INTERRUPT_AFTER_WRITE_SCRIPT: &str = r#"
const fs=require('fs');const readline=require('readline');const rl=readline.createInterface({input:process.stdin});
const send=o=>process.stdout.write(JSON.stringify(o)+'\n');let promptId;const marker=process.argv[2];
fs.appendFileSync(marker,'started\n');
rl.on('line',line=>{let m;try{m=JSON.parse(line)}catch{return}
if(m.method==='initialize')send({jsonrpc:'2.0',id:m.id,result:{protocolVersion:1,agentCapabilities:{},agentInfo:{name:'interrupt-fixture',version:'1'}}});
else if(m.method==='session/new')send({jsonrpc:'2.0',id:m.id,result:{sessionId:'interrupt-session',configOptions:[{id:'model',name:'Model',category:'model',type:'select',currentValue:'fixture/beta',options:[{value:'fixture/beta',name:'Beta'}]}]}});
else if(m.method==='session/set_config_option'&&m.params.value==='fixture/beta')send({jsonrpc:'2.0',id:m.id,result:{configOptions:[{id:'model',name:'Model',category:'model',type:'select',currentValue:'fixture/beta',options:[{value:'fixture/beta',name:'Beta'}]}]}});
else if(m.method==='session/prompt'){promptId=m.id;fs.appendFileSync(marker,'prompt:'+m.params.prompt[0].text+'\n');send({jsonrpc:'2.0',id:'interrupt-write',method:'fs/write_text_file',params:{sessionId:'interrupt-session',path:process.cwd().replace(/\\/g,'/')+'/stage-two.txt',content:'second effect\n'}})}
else if(m.id==='interrupt-write')fs.appendFileSync(marker,'acted\n');
});
"#;

#[test]
fn isolated_review_applies_only_the_saved_digest_and_clean_baseline() {
    let Some(fixture) = Fixture::new("review") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let make_review = || {
        let output = fixture.run(&["delegate", "Create stage-one.txt"]);
        assert!(output.status.success(), "{}", output_text(&output));
        let report = output_text(&output);
        let id = report
            .lines()
            .find_map(|line| line.strip_prefix("task: "))
            .unwrap()
            .to_owned();
        let preview = fixture.run(&["review", &id]);
        assert!(preview.status.success(), "{}", output_text(&preview));
        let preview = output_text(&preview);
        assert!(
            preview.contains("+first effect") && preview.contains("verifier    proven"),
            "{preview}"
        );
        let digest = preview
            .lines()
            .find_map(|line| line.strip_prefix("sha256: "))
            .unwrap()
            .to_owned();
        (id, digest)
    };
    let (apply_id, digest) = make_review();
    let (discard_id, discard_digest) = make_review();
    let output = fixture.run(&["discard", &discard_id]);
    assert!(output.status.success(), "{}", output_text(&output));
    assert!(!fixture
        .run(&["apply", &discard_id, "--sha256", &discard_digest])
        .status
        .success());
    assert!(!fixture.workspace.join("stage-one.txt").exists());
    assert!(!fixture
        .run(&["apply", &apply_id, "--sha256", "wrong"])
        .status
        .success());
    let dirty = fixture.workspace.join("operator.txt");
    std::fs::write(&dirty, "operator edit").unwrap();
    let refused = fixture.run(&["apply", &apply_id, "--sha256", &digest]);
    assert!(!refused.status.success() && output_text(&refused).contains("target is dirty"));
    assert_eq!(std::fs::read_to_string(&dirty).unwrap(), "operator edit");
    assert!(!fixture.workspace.join("stage-one.txt").exists());
    std::fs::remove_file(dirty).unwrap();
    let review_file = fixture
        .data
        .join("artifacts")
        .join(&apply_id)
        .join("review.json");
    let original = std::fs::read(&review_file).unwrap();
    let mut altered: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let expected_tree = altered["tree"].as_str().unwrap();
    altered["tree"] = serde_json::Value::String("0".repeat(expected_tree.len()));
    std::fs::write(&review_file, serde_json::to_vec(&altered).unwrap()).unwrap();
    let refused = fixture.run(&["apply", &apply_id, "--sha256", &digest]);
    assert!(!refused.status.success());
    assert!(output_text(&refused).contains("does not reconstruct"));
    assert!(!fixture.workspace.join("stage-one.txt").exists());
    std::fs::write(review_file, original).unwrap();
    let applied = fixture.run(&["apply", &apply_id, "--sha256", &digest]);
    assert!(applied.status.success(), "{}", output_text(&applied));
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-one.txt"))
            .unwrap()
            .trim(),
        "first effect"
    );
    let preview = fixture.run(&["review", &apply_id]);
    assert!(output_text(&preview).contains("review: applied"));
}

#[test]
fn quota_after_known_edit_resumes_once_in_fresh_process_and_verifies() {
    let Some(fixture) = Fixture::new("quota") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let task_id = fixture.start_recoverable_task();
    let checkpoint = fixture.run(&["checkpoint", &task_id]);
    assert!(checkpoint.status.success(), "{}", output_text(&checkpoint));
    let checkpoint_text = output_text(&checkpoint);
    assert!(
        checkpoint_text.contains("Create stage-two.txt"),
        "{checkpoint_text}"
    );
    assert!(
        checkpoint_text.contains("stage-one.txt"),
        "{checkpoint_text}"
    );
    assert!(
        checkpoint_text.contains("JEAN task verdict: needs_attention"),
        "{checkpoint_text}"
    );

    let mut desktop = Command::new(env!("CARGO_BIN_EXE_jean"))
        .args([
            "desktop-request",
            "--channel",
            "0123456789abcdef0123456789abcdef",
            "--",
            &format!("/resume {task_id}"),
        ])
        .env("JEAN_DATA_DIR", &fixture.data)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let input = desktop.stdin.take().unwrap();
    let resumed = desktop.wait_with_output().unwrap();
    drop(input);
    assert!(resumed.status.success(), "{}", output_text(&resumed));
    assert!(
        output_text(&resumed).contains("result: succeeded"),
        "{}",
        output_text(&resumed)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-one.txt")).unwrap(),
        "first effect\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-two.txt")).unwrap(),
        "second effect\n"
    );
    let worker_trace = std::fs::read_to_string(fixture.root.join("beta-started.txt")).unwrap();
    assert!(
        worker_trace.contains("Create stage-two.txt"),
        "{worker_trace}"
    );
    assert!(
        !worker_trace.contains("Create stage one"),
        "context recovery must only prompt for remaining work: {worker_trace}"
    );
    let original = output_text(&fixture.run(&["task", &task_id]));
    let replacement = original
        .lines()
        .find_map(|line| line.strip_prefix("resumed as  "))
        .expect(&original);
    assert!(original.contains("checkpoint  resumed"), "{original}");
    let activity = output_text(&fixture.run(&["task", replacement]));
    for expected in [
        "worker      beta (acp)",
        "model       \"fixture/beta\"",
        "Writing the remaining file",
        "verifier    proven",
    ] {
        assert!(
            activity.contains(expected),
            "missing {expected}: {activity}"
        );
    }
    let settled = fixture.run(&["checkpoint", &task_id]);
    assert!(output_text(&settled).contains("resume attempts: 1/1"));
    let worker_trace = std::fs::read_to_string(fixture.root.join("beta-started.txt")).unwrap();
    assert!(worker_trace.starts_with("started\n"), "{worker_trace}");
    assert!(
        worker_trace.contains("Create stage-two.txt"),
        "{worker_trace}"
    );
    assert!(
        !worker_trace.contains("Create stage one"),
        "the replacement only receives checkpointed remaining work: {worker_trace}"
    );
    let settled = fixture.run(&["checkpoint", &task_id]);
    assert!(settled.status.success(), "{}", output_text(&settled));
    assert!(
        output_text(&settled).contains("replacement task verdict: succeeded"),
        "{}",
        output_text(&settled)
    );
    assert!(
        output_text(&settled).contains("resume attempts: 1/1"),
        "{}",
        output_text(&settled)
    );
}

#[test]
fn conflicting_workspace_blocks_replacement_worker() {
    let Some(fixture) = Fixture::new("conflict") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let task_id = fixture.start_recoverable_task();
    std::fs::write(
        fixture.workspace.join("stage-one.txt"),
        "operator changed this\n",
    )
    .unwrap();
    let resumed = fixture.run(&["resume", &task_id]);
    assert_eq!(resumed.status.code(), Some(4), "{}", output_text(&resumed));
    assert!(
        output_text(&resumed).contains("result: needs_attention"),
        "{}",
        output_text(&resumed)
    );
    assert!(
        !fixture.root.join("beta-started.txt").exists(),
        "replacement worker must not start"
    );
}

#[test]
fn context_exhaustion_resumes_remaining_work_without_repeating_first_edit() {
    let Some(fixture) = Fixture::new("context") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let task_id = fixture.start_recoverable_task();
    let status = fixture.run(&["checkpoint", &task_id]);
    assert!(status.status.success(), "{}", output_text(&status));
    assert!(
        output_text(&status).contains("context_exhausted"),
        "{}",
        output_text(&status)
    );

    let resumed = fixture.run(&["resume", &task_id]);
    assert!(resumed.status.success(), "{}", output_text(&resumed));
    assert!(
        output_text(&resumed).contains("result: succeeded"),
        "{}",
        output_text(&resumed)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-one.txt")).unwrap(),
        "first effect\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-two.txt")).unwrap(),
        "second effect\n"
    );
}

#[test]
fn production_routing_baseline_records_expected_and_observed_actions() {
    let mut matches = 0usize;

    for (tag, case, expected) in [
        ("quota", "quota", "resume_with_beta_remaining_work_verified"),
        (
            "context",
            "context",
            "fresh_session_beta_remaining_work_verified",
        ),
    ] {
        let Some(fixture) = Fixture::new(tag) else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_recoverable_task();
        let started = Instant::now();
        let output = fixture.run(&["resume", &task_id]);
        let elapsed = started.elapsed();
        let trace = std::fs::read_to_string(fixture.root.join("beta-started.txt"));
        let settled = fixture.checkpoint(&task_id);
        let observed = if output.status.success()
            && output_text(&output).contains("result: succeeded")
            && trace.as_ref().is_ok_and(|text| {
                text.starts_with("started\n")
                    && text.contains("Create stage-two.txt")
                    && !text.contains("Create stage one")
            })
            && settled["resumed_task_id"].as_str().is_some()
            && std::fs::read_to_string(fixture.workspace.join("stage-one.txt"))
                .is_ok_and(|content| content == "first effect\n")
            && std::fs::read_to_string(fixture.workspace.join("stage-two.txt"))
                .is_ok_and(|content| content == "second effect\n")
        {
            expected
        } else {
            "unsafe_or_unverified_action"
        };
        baseline_case(case, expected, observed, elapsed);
        matches += usize::from(expected == observed);
    }

    for (tag, case, expected) in [
        ("sign-in", "sign_in_pause", "pause_for_sign_in_remediation"),
        ("network", "network_pause", "pause_for_network_remediation"),
    ] {
        let Some(fixture) = Fixture::new(tag) else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_recoverable_task();
        let started = Instant::now();
        let output = fixture.run(&["resume", &task_id]);
        let elapsed = started.elapsed();
        let checkpoint = fixture.checkpoint(&task_id);
        let has_no_replacement = !fixture.root.join("beta-started.txt").exists();
        let expected_status = if tag == "sign-in" {
            "blocked_sign_in_expired"
        } else {
            "blocked_network_unavailable"
        };
        let checkpoint_blocker = if tag == "sign-in" {
            "sign_in_expired"
        } else {
            "network_unavailable"
        };
        let observed = if output.status.code() == Some(4)
            && output_text(&output).contains(expected_status)
            && checkpoint["blocked_on"].as_str() == Some(checkpoint_blocker)
            && checkpoint["resumable"] == true
            && has_no_replacement
        {
            expected
        } else {
            "replacement_started_or_blocker_lost"
        };
        baseline_case(case, expected, observed, elapsed);
        matches += usize::from(expected == observed);
    }

    {
        let Some(fixture) = Fixture::new("disk") else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_recoverable_task();
        let config_path = fixture.data.join("jean.toml");
        let config = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(
            &config_path,
            config.replace(
                "resume_min_free_disk_bytes=1",
                "resume_min_free_disk_bytes=1099511627776",
            ),
        )
        .unwrap();
        let started = Instant::now();
        let output = fixture.run(&["resume", &task_id]);
        let elapsed = started.elapsed();
        let checkpoint = fixture.checkpoint(&task_id);
        let expected = "pause_until_disk_space_available";
        let observed = if output_text(&output).contains("blocked_low_disk")
            && checkpoint["blocked_on"].as_str() == Some("low_disk")
            && checkpoint["attempts"] == 0
            && !fixture.root.join("beta-started.txt").exists()
        {
            expected
        } else {
            "replacement_started_or_attempt_consumed"
        };
        baseline_case("low_disk", expected, observed, elapsed);
        matches += usize::from(expected == observed);
    }

    {
        let Some(fixture) = Fixture::new("conflict") else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_recoverable_task();
        std::fs::write(
            fixture.workspace.join("stage-one.txt"),
            "operator changed this\n",
        )
        .unwrap();
        let started = Instant::now();
        let output = fixture.run(&["resume", &task_id]);
        let elapsed = started.elapsed();
        let expected = "needs_attention_workspace_evidence_conflict";
        let observed = if output.status.code() == Some(4)
            && output_text(&output).contains("result: needs_attention")
            && !fixture.root.join("beta-started.txt").exists()
        {
            expected
        } else {
            "replacement_started_despite_workspace_conflict"
        };
        baseline_case("changed_workspace_evidence", expected, observed, elapsed);
        matches += usize::from(expected == observed);
    }

    {
        let Some(fixture) = Fixture::new("uncertain") else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_recoverable_task();
        let path = fixture
            .data
            .join("checkpoints")
            .join(format!("{task_id}.json"));
        let mut checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        checkpoint["uncertain"] = true.into();
        checkpoint["resumable"] = false.into();
        checkpoint["attempts"] = 1.into();
        checkpoint["attempted_workers"] = serde_json::json!(["alpha", "beta"]);
        checkpoint["next_action"] = "replacement worker may already have acted".into();
        std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let started = Instant::now();
        let output = fixture.run(&["resume", &task_id]);
        let elapsed = started.elapsed();
        let after = fixture.checkpoint(&task_id);
        let expected = "human_review_uncertain_reservation";
        let observed = if output.status.code() == Some(4)
            && output_text(&output).contains("may already have acted")
            && after["uncertain"] == true
            && after["attempts"] == 1
            && !fixture.root.join("beta-started.txt").exists()
        {
            expected
        } else {
            "reservation_cleared_or_work_replayed"
        };
        baseline_case("uncertain_replacement", expected, observed, elapsed);
        matches += usize::from(expected == observed);
    }

    for (tag, case, expected) in [
        (
            "decision-no-fallback",
            "no_eligible_worker",
            "abstain_needs_attention_no_worker",
        ),
        (
            "decision-unknown",
            "unknown_failure",
            "abstain_unclassified_no_worker",
        ),
    ] {
        let Some(fixture) = Fixture::new(tag) else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_task();
        let before = fixture.checkpoint(&task_id);
        let started = Instant::now();
        let output = fixture.run(&["resume", &task_id]);
        let elapsed = started.elapsed();
        let after = fixture.checkpoint(&task_id);
        let observed = if output.status.code() == Some(4)
            && output_text(&output).contains("result: needs_attention")
            && !fixture.root.join("beta-started.txt").exists()
            && if tag == "decision-no-fallback" {
                before["eligible_free_workers"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
            } else {
                before["resumable"] == false
                    && before["results"].as_array().is_some_and(|results| {
                        results.iter().any(|result| {
                            result
                                .as_str()
                                .is_some_and(|value| value.contains("worker stopped"))
                        })
                    })
            }
            && after["attempts"] == 0
        {
            expected
        } else {
            "unsafe_replacement_forbidden_case_started"
        };
        baseline_case(case, expected, observed, elapsed);
        matches += usize::from(expected == observed);
    }

    println!("decision_baseline accuracy={matches}/9");
    assert_eq!(matches, 9, "all deterministic routing cases must match");
}

#[test]
fn sign_in_and_network_blockers_require_operator_remediation_before_resume() {
    for (tag, status, action) in [
        (
            "sign-in",
            "blocked_sign_in_expired",
            "sign in to the configured worker",
        ),
        (
            "network",
            "blocked_network_unavailable",
            "restore network access",
        ),
    ] {
        let Some(fixture) = Fixture::new(tag) else {
            eprintln!("skipped: node and git are required for the local ACP fixture");
            return;
        };
        let task_id = fixture.start_recoverable_task();
        let resumed = fixture.run(&["resume", &task_id]);
        assert_eq!(resumed.status.code(), Some(4), "{}", output_text(&resumed));
        assert!(
            output_text(&resumed).contains(status),
            "{}",
            output_text(&resumed)
        );
        assert!(
            output_text(&resumed).contains(action),
            "{}",
            output_text(&resumed)
        );
        assert!(!fixture.root.join("beta-started.txt").exists());
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("stage-one.txt")).unwrap(),
            "first effect\n"
        );
        let checkpoint = fixture.run(&["checkpoint", &task_id]);
        let text = output_text(&checkpoint);
        assert!(text.contains(status), "{text}");
        assert!(text.contains("resumable: true"), "{text}");
        assert!(text.contains("resume attempts: 0/1"), "{text}");
        let cleared = fixture.run(&["resume", &task_id, "--condition-cleared"]);
        assert!(cleared.status.success(), "{}", output_text(&cleared));
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("stage-two.txt")).unwrap(),
            "second effect\n"
        );
    }
}

#[test]
fn low_disk_preserves_retry_until_space_is_available() {
    let Some(fixture) = Fixture::new("disk") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let task_id = fixture.start_recoverable_task();
    let config_path = fixture.data.join("jean.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        config.replace(
            "resume_min_free_disk_bytes=1",
            "resume_min_free_disk_bytes=1099511627776",
        ),
    )
    .unwrap();

    for _ in 0..2 {
        let blocked = fixture.run(&["resume", &task_id]);
        assert_eq!(blocked.status.code(), Some(4), "{}", output_text(&blocked));
        assert!(
            output_text(&blocked).contains("blocked_low_disk"),
            "{}",
            output_text(&blocked)
        );
        assert!(!fixture.root.join("beta-started.txt").exists());
    }
    let checkpoint = fixture.run(&["checkpoint", &task_id]);
    let text = output_text(&checkpoint);
    assert!(text.contains("resumable: true"), "{text}");
    assert!(text.contains("resume attempts: 0/1"), "{text}");
    assert!(text.contains("blocked on: blocked_low_disk"), "{text}");

    std::fs::write(
        &config_path,
        std::fs::read_to_string(&config_path).unwrap().replace(
            "resume_min_free_disk_bytes=1099511627776",
            "resume_min_free_disk_bytes=1",
        ),
    )
    .unwrap();
    let resumed = fixture.run(&["resume", &task_id]);
    assert!(resumed.status.success(), "{}", output_text(&resumed));
    assert!(output_text(&resumed).contains("result: succeeded"));
}

#[test]
fn uncertain_replacement_reservation_is_never_cleared_after_process_exit() {
    let Some(fixture) = Fixture::new("uncertain-crash") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let task_id = fixture.start_recoverable_task();
    std::fs::write(
        fixture.root.join("beta.js"),
        BETA_INTERRUPT_AFTER_WRITE_SCRIPT,
    )
    .unwrap();
    // This fixture waits for a real write acknowledgement through Git,
    // verification, and ACP. Leave headroom for the other Windows fixtures
    // running concurrently, including inside the replacement's watchdog.
    let config_path = fixture.data.join("jean.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        config_path,
        config
            .replace("session_timeout_secs=30", "session_timeout_secs=120")
            .replace("idle_timeout_secs=10", "idle_timeout_secs=60"),
    )
    .unwrap();
    let log_path = fixture.root.join("replacement.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut replacement = Command::new(env!("CARGO_BIN_EXE_jean"))
        .args(["resume", &task_id])
        .env("JEAN_DATA_DIR", &fixture.data)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("start replacement in a fresh JEAN process");
    let marker = fixture.root.join("beta-started.txt");
    let deadline = Instant::now() + std::time::Duration::from_secs(90);
    let mut replacement_acted = false;
    while Instant::now() < deadline {
        if std::fs::read_to_string(&marker)
            .is_ok_and(|trace| trace.lines().any(|line| line == "acted"))
        {
            replacement_acted = true;
            break;
        }
        if replacement.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !replacement_acted {
        let early_exit = replacement.try_wait().unwrap();
        let _ = replacement.kill();
        let _ = replacement.wait();
        let trace = std::fs::read_to_string(&marker).unwrap_or_default();
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        panic!(
            "replacement did not acknowledge its write within 90 seconds +             (early exit: {early_exit:?})\nworker trace:\n{trace}\nJEAN output:\n{log}"
        );
    }
    // Simulate JEAN being interrupted after the replacement's write was applied but before the
    // worker returned a final response or the checkpoint could be settled.
    replacement
        .kill()
        .expect("interrupt the uncertain replacement");
    let _ = replacement.wait();

    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-one.txt")).unwrap(),
        "first effect\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("stage-two.txt")).unwrap(),
        "second effect\n"
    );
    let checkpoint = fixture.checkpoint(&task_id);
    assert_eq!(checkpoint["uncertain"], true);
    assert_eq!(checkpoint["resumable"], false);
    assert_eq!(checkpoint["attempts"], 1);
    assert_eq!(
        checkpoint["attempted_workers"],
        serde_json::json!(["alpha", "beta"])
    );

    let resumed = fixture.run(&["resume", &task_id]);
    assert_eq!(resumed.status.code(), Some(4), "{}", output_text(&resumed));
    assert!(
        output_text(&resumed).contains("result: needs_attention"),
        "{}",
        output_text(&resumed)
    );
    let trace = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(
        trace.lines().filter(|line| *line == "started").count(),
        1,
        "{trace}"
    );
    assert!(trace.lines().any(|line| line == "acted"), "{trace}");
    assert!(trace.contains("Create stage-two.txt"), "{trace}");
    assert!(!trace.contains("Create stage one"), "{trace}");
    let activity = output_text(&fixture.run(&["task", &task_id]));
    assert!(
        activity.contains("checkpoint  uncertain; review required before replay"),
        "{activity}"
    );
    assert!(activity.contains("verifier    not_proven"), "{activity}");
    let after = fixture.checkpoint(&task_id);
    assert_eq!(after["uncertain"], true);
    assert_eq!(after["attempts"], 1);
    assert_eq!(after["resumable"], false);
}

fn run_profile(
    fixture: &Fixture,
    profile: &str,
    instruction: Option<&str>,
) -> (String, String, String) {
    let marker = fixture.root.join("beta-started.txt");
    let verifier = fixture.root.join("verify-runs.txt");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&verifier);
    let workspace = fixture.workspace.display().to_string();
    let mut args = vec![
        "goal",
        "Build the dashboard frontend",
        "--workspace",
        &workspace,
        "--worker",
        "alpha",
        "--profile",
        profile,
    ];
    if let Some(instruction) = instruction {
        args.extend(["--instruction", instruction]);
    }
    let output = fixture.run(&args);
    assert_eq!(output.status.code(), Some(0), "{}", output_text(&output));
    let task_id = output_text(&output)
        .lines()
        .find_map(|line| line.strip_prefix("task: ").map(str::to_owned))
        .expect("goal reports task id");
    let status = fixture.run(&["task", &task_id]);
    assert!(status.status.success(), "{}", output_text(&status));
    let worker = std::fs::read_to_string(marker).expect("worker captured its prompt");
    let checks = std::fs::read_to_string(verifier).expect("configured verifier ran");
    assert_eq!(
        checks.lines().filter(|line| *line == "run").count(),
        2,
        "baseline and post-worker checks must run"
    );
    if fixture.root.to_string_lossy().contains("profile-quick") {
        let expected = if profile == "quick-prototype" {
            "quick"
        } else {
            "polished"
        };
        assert_eq!(checks, format!("{expected}\nrun\n{expected}\nrun\n"));
    }
    (task_id, output_text(&status), worker)
}

#[test]
fn desktop_goal_routes_to_worker_and_requires_a_matching_approval() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let Some(fixture) = Fixture::new("profile-quick") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let config = fixture.data.join("jean.toml");
    let mut content = std::fs::read_to_string(&config).unwrap();
    content.push_str("\n[policy]\nrequire_approval=['agent.delegate']\napproval_timeout_secs=10\n");
    std::fs::write(config, content).unwrap();

    const CHANNEL: &str = "0123456789abcdef0123456789abcdef";
    let mut child = Command::new(env!("CARGO_BIN_EXE_jean"))
        .args(["desktop-request", "--channel", CHANNEL, "--workspace"])
        .arg(&fixture.workspace)
        .args([
            "--profile",
            "quick-prototype",
            "--",
            "Start with alpha. Build the dashboard frontend",
        ])
        .env("JEAN_DATA_DIR", &fixture.data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut task_id = None;
    let mut requested = false;
    let mut verified = false;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if let Some(body) = line.strip_prefix(&format!("JEAN:{CHANNEL} ")) {
            let message: serde_json::Value = serde_json::from_str(body).unwrap();
            match message["kind"].as_str() {
                Some("task") => task_id = message["id"].as_str().map(str::to_owned),
                Some("approval") => {
                    assert!(task_id.is_some(), "task ID is available before approval");
                    let id = message["id"].as_u64().unwrap();
                    writeln!(
                        input,
                        "{}",
                        serde_json::json!({"kind":"approve","id":id + 1,"allow":false})
                    )
                    .unwrap();
                    writeln!(
                        input,
                        "{}",
                        serde_json::json!({"kind":"approve","id":id,"allow":true})
                    )
                    .unwrap();
                    input.flush().unwrap();
                    requested = true;
                }
                _ => {}
            }
        }
        verified |= line.starts_with("result: succeeded");
    }
    assert!(child.wait().unwrap().success());
    assert!(requested && verified);
    let status = fixture.run(&["task", &task_id.unwrap()]);
    assert!(output_text(&status).contains("latest step"));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("verify-runs.txt")).unwrap(),
        "quick\nrun\nquick\nrun\n"
    );
}

#[test]
fn profiles_change_worker_guidance_and_explicit_instruction_overrides_it() {
    let Some(fixture) = Fixture::new("profile-quick") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let (_, quick_status, quick_worker) = run_profile(&fixture, "quick-prototype", None);
    assert!(quick_status.contains("quick prototype"), "{quick_status}");
    assert!(
        quick_status.contains("Keep the change minimal"),
        "{quick_status}"
    );
    assert!(
        quick_worker.contains("Keep the change minimal"),
        "{quick_worker}"
    );

    let (_, polished_status, polished_worker) = run_profile(&fixture, "polished-ui", None);
    assert!(polished_status.contains("polished UI"), "{polished_status}");
    assert!(
        polished_status.contains("subtle motion"),
        "{polished_status}"
    );
    assert!(
        polished_worker.contains("subtle motion"),
        "{polished_worker}"
    );
    assert_ne!(quick_worker, polished_worker);

    let (_, override_status, override_worker) =
        run_profile(&fixture, "polished-ui", Some("no animation"));
    assert!(override_status.contains("polished UI"), "{override_status}");
    assert!(
        override_status.contains("no animation (explicit override)"),
        "{override_status}"
    );
    assert!(
        !override_status.contains("subtle motion"),
        "{override_status}"
    );
    assert!(
        override_worker.contains("no animation (explicit override)"),
        "{override_worker}"
    );
    assert!(
        !override_worker.contains("subtle motion"),
        "{override_worker}"
    );
}

#[test]
fn learned_preferences_are_workspace_scoped_explicit_and_forgettable() {
    let Some(fixture) = Fixture::new("profile-quick") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let (task_a, _, _) = run_profile(&fixture, "quick-prototype", None);
    let (task_b, _, _) = run_profile(&fixture, "quick-prototype", None);
    let workspace = fixture.workspace.display().to_string();
    let unknown = fixture.run(&[
        "preference",
        "feedback",
        &workspace,
        "not-a-recorded-task",
        "no-animation",
    ]);
    assert!(!unknown.status.success(), "{}", output_text(&unknown));
    for task_id in [&task_a, &task_b] {
        let out = fixture.run(&[
            "preference",
            "feedback",
            &workspace,
            task_id,
            "no-animation",
        ]);
        assert!(out.status.success(), "{}", output_text(&out));
    }
    let status = fixture.run(&["preference", "status", &workspace]);
    assert!(output_text(&status).contains("candidate: true"));
    assert!(output_text(&status).contains(&task_a));
    let accepted = fixture.run(&["preference", "accept", &workspace]);
    assert!(accepted.status.success(), "{}", output_text(&accepted));

    let plain_goal = fixture.run(&[
        "goal",
        "Build the dashboard frontend",
        "--workspace",
        &workspace,
        "--worker",
        "alpha",
    ]);
    assert_eq!(
        plain_goal.status.code(),
        Some(0),
        "{}",
        output_text(&plain_goal)
    );
    let plain_prompt = std::fs::read_to_string(fixture.root.join("beta-started.txt")).unwrap();
    assert!(
        plain_prompt.contains("Respect the accepted workspace preference: no animation."),
        "{plain_prompt}"
    );
    let plain_task = output_text(&plain_goal)
        .lines()
        .find_map(|line| line.strip_prefix("task: ").map(str::to_owned))
        .unwrap();
    let plain_status = fixture.run(&["task", &plain_task]);
    assert!(output_text(&plain_status)
        .contains("Respect the accepted workspace preference: no animation."));

    let (_, _, quick_with_preference) = run_profile(&fixture, "quick-prototype", None);
    assert!(
        quick_with_preference.contains("Accepted workspace preference: no animation."),
        "{quick_with_preference}"
    );

    let (task_c, _, prompt) = run_profile(&fixture, "polished-ui", None);
    assert!(
        prompt.contains("no animation (accepted workspace preference)"),
        "{prompt}"
    );
    assert!(!prompt.contains("subtle motion"), "{prompt}");
    let task_status = fixture.run(&["task", &task_c]);
    assert!(output_text(&task_status).contains("no animation (accepted workspace preference)"));

    let (_, _, explicit_conflict) = run_profile(&fixture, "polished-ui", Some("allow animation"));
    assert!(
        explicit_conflict.contains("allow animation"),
        "{explicit_conflict}"
    );
    assert!(
        !explicit_conflict.contains("accepted workspace preference"),
        "{explicit_conflict}"
    );

    let disabled = fixture.run(&["preference", "learning", &workspace, "--enabled", "false"]);
    assert!(disabled.status.success(), "{}", output_text(&disabled));
    let (_, _, while_disabled) = run_profile(&fixture, "polished-ui", None);
    assert!(
        while_disabled.contains("no animation (accepted workspace preference)"),
        "accepted preferences remain active when new learning is disabled: {while_disabled}"
    );
    let rejected_feedback = fixture.run(&[
        "preference",
        "feedback",
        &workspace,
        "task-after-disable",
        "no-animation",
    ]);
    assert!(
        !rejected_feedback.status.success(),
        "{}",
        output_text(&rejected_feedback)
    );
    fixture.run(&["preference", "learning", &workspace, "--enabled", "true"]);
    let corrected = fixture.run(&["preference", "correct", &workspace, "no-animation"]);
    assert!(corrected.status.success(), "{}", output_text(&corrected));
    assert!(output_text(&corrected).contains("accepted no-animation: true"));

    let (_, _, explicit) = run_profile(&fixture, "polished-ui", Some("no animation"));
    assert!(
        explicit.contains("no animation (explicit override)"),
        "{explicit}"
    );
    assert!(!explicit.contains("subtle motion"), "{explicit}");

    let other = fixture.root.join("other-workspace");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("base.txt"), "base\n").unwrap();
    let git = jean_core::resolve::resolve_executable(
        "git",
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
    .unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "."][..],
        &[
            "-c",
            "user.email=fixture@jean",
            "-c",
            "user.name=fixture",
            "commit",
            "-qm",
            "base",
        ][..],
    ] {
        assert!(Command::new(&git)
            .args(args)
            .current_dir(&other)
            .status()
            .unwrap()
            .success());
    }
    let other_workspace = other.display().to_string();
    let wrong_scope = fixture.run(&[
        "preference",
        "feedback",
        &other_workspace,
        &task_a,
        "no-animation",
    ]);
    assert!(
        !wrong_scope.status.success(),
        "{}",
        output_text(&wrong_scope)
    );
    assert!(output_text(&wrong_scope).contains("completed delegation in this workspace"));
    let other_output = fixture.run(&[
        "goal",
        "Build the dashboard frontend",
        "--workspace",
        &other_workspace,
        "--worker",
        "alpha",
        "--profile",
        "polished-ui",
    ]);
    assert_eq!(
        other_output.status.code(),
        Some(0),
        "{}",
        output_text(&other_output)
    );
    let other_output_text = output_text(&other_output);
    let other_task = other_output_text
        .lines()
        .find_map(|line| line.strip_prefix("task: ").map(str::to_owned))
        .unwrap();
    let other_prompt = std::fs::read_to_string(fixture.root.join("beta-started.txt")).unwrap();
    let _other_task = other_task;
    assert!(other_prompt.contains("subtle motion"), "{other_prompt}");
    assert!(
        !other_prompt.contains("accepted workspace preference"),
        "{other_prompt}"
    );

    let forgotten = fixture.run(&["preference", "forget", &workspace]);
    assert!(forgotten.status.success(), "{}", output_text(&forgotten));
    let (_, _, after_forget) = run_profile(&fixture, "polished-ui", None);
    assert!(after_forget.contains("subtle motion"), "{after_forget}");
    assert!(
        !after_forget.contains("accepted workspace preference"),
        "{after_forget}"
    );
    let verifier_runs = std::fs::read_to_string(fixture.root.join("verify-runs.txt")).unwrap();
    assert!(
        verifier_runs.lines().count() >= 2,
        "configured verifier ran"
    );
}

#[test]
fn worker_success_claim_does_not_override_failing_workspace_verification() {
    let Some(fixture) = Fixture::new("profile-fail") else {
        eprintln!("skipped: node and git are required for the local ACP fixture");
        return;
    };
    let workspace = fixture.workspace.display().to_string();
    let output = fixture.run(&[
        "goal",
        "Build the dashboard frontend",
        "--workspace",
        &workspace,
        "--worker",
        "alpha",
        "--profile",
        "polished-ui",
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", output_text(&output));
    let report = output_text(&output);
    assert!(report.contains("result: failed"), "{report}");
    let task_id = report
        .lines()
        .find_map(|line| line.strip_prefix("task: "))
        .expect(&report);
    let activity = output_text(&fixture.run(&["task", task_id]));
    assert!(activity.contains("verifier    not_proven"), "{activity}");
    assert!(
        activity.contains("model       confirmation not recorded"),
        "{activity}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("verify-runs.txt"))
            .unwrap()
            .lines()
            .count(),
        2,
        "the verifier runs before and after the worker"
    );
}
