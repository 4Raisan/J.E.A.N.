//! Phase-2 checks against the **real `jean.exe`**: what the shipped binary reports about its
//! transport, and what `jean do` does with the authority its operator carries.
//!
//! `jean-core`'s scenarios drive the engine with a fake provider and a scripted prompter. These
//! tests cover the three things only a spawned binary can show: the strings an operator actually
//! reads, the clap surface of the flags (an unrecognised flag fails differently from a refused
//! action), and that a build with `provider = "none"` still behaves exactly as Phase 1 left it.
//! Nothing here starts or contacts a model server.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const TOOLS: &str = r#"
config_version = 1

[[tool]]
id = "demo.echo"
kind = "shell"
display_name = "Demo echo"
capabilities = ["shell"]
command = "cmd.exe"
args = ["/c", "echo JEAN_DEMO_OK"]
tier = "read_only"
timeout_secs = 30
verify = { method = "output_contains", expected = 0, needle = "JEAN_DEMO_OK" }
"#;

#[derive(Debug)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn text(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// A scratch data directory whose `[model]` block is chosen per test.
struct Harness {
    dir: PathBuf,
}

impl Harness {
    fn new(tag: &str, model: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("jean-cli2-test-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch data directory");
        fs::write(
            dir.join("jean.toml"),
            format!(
                "config_version = 1\n\n[core]\ndata_dir = \"{}\"\n\n[tools]\nregistry_path = \
                 \"{}\"\n\n{model}",
                dir.display().to_string().replace('\\', "/"),
                dir.join("tools.toml")
                    .display()
                    .to_string()
                    .replace('\\', "/"),
            ),
        )
        .expect("write jean.toml");
        fs::write(dir.join("tools.toml"), TOOLS).expect("write tools.toml");
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_jean"))
            .current_dir(&self.dir)
            .env("JEAN_DATA_DIR", &self.dir)
            .args(args)
            .output()
            .expect("`jean.exe` must be spawnable — it is the binary cargo just built");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_binary_reports_a_verified_https_transport_without_naming_a_credential_value() {
    const NAME: &str = "JEAN_CLI2_TEST_CREDENTIAL";
    // The *value* is the thing that must never appear in any output.
    std::env::set_var(NAME, "sk-cli2-very-secret-value");
    let harness = Harness::new(
        "transport",
        &format!(
            "[model]\nprovider = \"ollama\"\nbase_url = \
             \"http://127.0.0.1:11434\"\nmodel = \"fixture-model\"\napi_key_env = \
             \"{NAME}\"\n"
        ),
    );

    let run = harness.run(&["model", "status"]);
    assert_eq!(run.code, 0, "{}", run.text());
    let text = run.text();
    assert!(
        text.contains("verified") && text.contains("https"),
        "the transport line must state that HTTPS is verified: {text}"
    );
    assert!(
        !text.contains("no TLS client") && !text.contains("does not ship"),
        "the binary still claims it cannot do TLS: {text}"
    );
    assert!(
        text.contains("chunked"),
        "the supported-subset line must say chunked answers are accepted: {text}"
    );
    assert!(
        !text.contains("sk-cli2-very-secret-value"),
        "the credential value reached the terminal"
    );
    assert!(
        text.contains(NAME),
        "the variable's NAME is the useful diagnostic: {text}"
    );
    std::env::remove_var(NAME);
}

#[test]
fn provider_none_refuses_planning_and_leaves_every_deterministic_command_alone() {
    let harness = Harness::new("provider-none", "[model]\nprovider = \"none\"\n");

    let refused = harness.run(&["do", "echo something that is not registered"]);
    assert_eq!(
        refused.code, 1,
        "a run with no provider refuses: {refused:?}"
    );
    assert!(
        refused.text().contains("none"),
        "and says which setting: {}",
        refused.text()
    );

    // Phase 1's path, in the same build, still runs and still verifies.
    let deterministic = harness.run(&["exec", "demo.echo"]);
    assert_eq!(deterministic.code, 0, "{}", deterministic.text());
    assert!(
        deterministic.text().contains("proven"),
        "the verdict column is unchanged: {}",
        deterministic.text()
    );
}

#[test]
fn prompt_each_is_a_real_flag_of_jean_do_and_an_unknown_one_is_still_rejected() {
    let harness = Harness::new("prompt-each", "[model]\nprovider = \"none\"\n");

    // If `--prompt-each` were missing from the clap surface, this would be exit 2 ("unexpected
    // argument"), not the provider refusal.
    let with_flag = harness.run(&["do", "--prompt-each", "do the thing"]);
    assert_eq!(
        with_flag.code,
        1,
        "--prompt-each parsed, then the absent provider was refused: {}",
        with_flag.text()
    );
    assert!(
        !with_flag.text().contains("unexpected"),
        "--prompt-each is not a known flag: {}",
        with_flag.text()
    );

    let without = harness.run(&["do", "do the thing"]);
    assert_eq!(
        without.code, with_flag.code,
        "the flag changes who is asked, not what is refused"
    );

    // The control: an invented flag really is rejected, so the assertion above means something.
    let invented = harness.run(&["do", "--not-a-flag", "x"]);
    assert_eq!(
        invented.code, 2,
        "clap still rejects unknown arguments: {invented:?}"
    );
}
