//! A browser J.E.A.N. starts, owns and closes — decision D-046.
//!
//! ## What this crate is, in one sentence
//!
//! It launches the Chrome (or Edge, or Brave) that is **already installed** as a supervised child of
//! J.E.A.N., with a profile of its own that it created and an empty beginning, on a debugging endpoint
//! bound to loopback, and it speaks CDP to that process and no other.
//!
//! ## What it refuses to be
//!
//! * **Not an attachment.** There is no code path here that connects to a browser somebody else started.
//!   A running Chrome's debugging port, if it has one, belongs to that person's session, cookies and
//!   signed-in accounts; driving it would act *as* them. The endpoint this crate dials is one this crate
//!   caused to exist, on a port it chose, on a profile it made.
//! * **Not a second execution path.** The child is spawned by [`jean_process::piped`], so it is born
//!   inside a kill-on-close job object like every other worker in this project: its renderers and GPU
//!   process are contained with it, [`Control::terminate`] reaches all of them, and a panic in the middle
//!   of a workflow cannot leave a browser running on the operator's desktop. `headless_chrome` could
//!   spawn this process itself; it is used only as the CDP client, because a browser J.E.A.N. did not
//!   start is a browser J.E.A.N. cannot be trusted to stop.
//! * **Not a downloader.** The `fetch` feature of that crate — which installs a Chromium build — is not
//!   enabled and must stay that way. Detection that finds nothing is a reported blocker.
//! * **Not unlimited.** Every read goes through [`BrowserConfig`]: `max_page_bytes` bounds what a page
//!   can hand back, `startup_timeout_secs` bounds how long the endpoint may take to appear, and
//!   `action_timeout_secs` is passed to the CDP client as its default call timeout.
//!
//! ## The endpoint is validated before it is dialled
//!
//! Chrome prints `DevTools listening on ws://127.0.0.1:<port>/devtools/browser/<id>` on stderr. That
//! line is the only source of the URL, and the URL is checked — scheme `ws`, host loopback — before a
//! connection is attempted. A browser that printed a routable address would be refused rather than
//! followed, because the thing being dialled is a protocol with total control over the process.

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use headless_chrome::Browser as CdpBrowser;
use headless_chrome::Tab;
use headless_chrome::protocol::cdp::Page;

use jean_process::piped::{Control, PipedChild};
use jean_process::spec::{LaunchSpec, TerminalMode};
use jean_protocol::config::BrowserConfig;

/// The flags this crate adds to the ones it needs, chosen for containment rather than for speed.
///
/// `--enable-automation` is deliberately absent from the usual set: it only draws a banner, and a banner
/// is a thing on the operator's screen that they did not ask for.
const SAFETY_ARGS: &[&str] = &[
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-component-extensions-with-background-pages",
    "--disable-features=TranslateUI",
    "--disable-hang-monitor",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--no-default-browser-check",
    "--mute-audio",
    "--hide-scrollbars",
    "--password-store=basic",
    "--use-mock-keychain",
];

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// The child could not be started, or died before it spoke.
    #[error("the browser child could not be started: {reason}")]
    Launch { reason: String },
    /// The debugging endpoint never appeared, or appeared as something this crate will not dial.
    #[error("no usable loopback debugging endpoint: {reason}")]
    Endpoint { reason: String },
    /// The CDP call failed: a timeout, a missing element, a page that went away.
    #[error("the browser refused the action: {reason}")]
    Action { reason: String },
    /// A navigation the configuration does not cover.
    #[error("{url} is not covered by browser.allowed_origins ({allowed:?})")]
    OriginDenied { url: String, allowed: String },
    #[error(
        "the browser profile {0} is not empty; J.E.A.N. only drives a profile it started itself"
    )]
    ProfileNotEmpty(PathBuf),
    #[error("the browser path {0} is not a file that exists")]
    MissingExecutable(PathBuf),
}

pub type Result<T> = std::result::Result<T, BrowserError>;

impl From<anyhow::Error> for BrowserError {
    fn from(e: anyhow::Error) -> Self {
        BrowserError::Action {
            reason: e.to_string(),
        }
    }
}

/// The scheme, host and port of a URL, as the allowlist knows them.
///
/// Parsed here rather than by `Url` because the answer is only ever compared against a small set of
/// strings, and a full parse would turn a rejected URL into an error path of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

impl Origin {
    /// The origin of an absolute URL, or `None` when it has no scheme to ask about.
    pub fn of(url: &str) -> Option<Origin> {
        let (scheme, rest) = url.split_once("://")?;
        let authority = rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .rsplit('@')
            .next()
            .unwrap_or_default();
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse::<u16>().ok()),
            None => (authority.to_owned(), None),
        };
        Some(Origin {
            scheme: scheme.to_ascii_lowercase(),
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Whether this origin is one the operator listed.
    ///
    /// An entry with no port covers every port on that host — a local fixture server does not get to
    /// choose a port the operator would have to predict. A bare scheme (`file`) covers the scheme,
    /// which is the only meaningful bound for a path the operator pointed at themselves.
    pub fn is_allowed_by(&self, allowed: &[String]) -> bool {
        allowed.iter().any(|entry| {
            let entry = entry.trim();
            if let Some((scheme, authority)) = entry.split_once("://") {
                let (host, port) = match authority.rsplit_once(':') {
                    Some((h, p)) => (h.to_ascii_lowercase(), p.parse::<u16>().ok()),
                    None => (authority.to_ascii_lowercase(), None),
                };
                self.scheme == scheme.to_ascii_lowercase()
                    && self.host == host
                    && port.is_none_or(|want| self.port == Some(want))
            } else {
                // A bare scheme (`file`): there is no host to check, so the scheme is the whole bound.
                self.scheme == entry.to_ascii_lowercase()
            }
        })
    }
}

/// The debugging endpoint, once it has been read and checked.
fn endpoint_is_loopback(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://") else {
        return false;
    };
    let host = rest
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    host.eq_ignore_ascii_case("127.0.0.1")
        || host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("[::1]")
        || host == "::1"
}

/// A browser this process started, with the handles that make that claim true.
///
/// Dropping it ends the browser: the job object is kill-on-close and [`OwnedBrowser::close`] terminates
/// it explicitly so the exit code can be reported rather than inferred.
pub struct OwnedBrowser {
    child: Option<PipedChild>,
    control: Control,
    cdp: CdpBrowser,
    tab: Option<Arc<Tab>>,
    profile: PathBuf,
    action_timeout: Duration,
    ws_url: String,
    stderr_pump: Option<JoinHandle<()>>,
}

impl OwnedBrowser {
    /// Start the browser at `executable`, in `profile`, and connect to the endpoint it reports.
    ///
    /// `profile` must already exist and be **empty**: the check is what stops a mis-typed
    /// `browser_path`-adjacent setting from pointing J.E.A.N. at a real person's profile, which is the
    /// one mistake a browser driver must not be able to make.
    pub fn start(
        config: &BrowserConfig,
        executable: &Path,
        profile: &Path,
        extra_args: &[String],
    ) -> Result<Self> {
        if !executable.is_file() {
            return Err(BrowserError::MissingExecutable(executable.to_path_buf()));
        }
        let not_empty = std::fs::read_dir(profile)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true);
        if not_empty {
            return Err(BrowserError::ProfileNotEmpty(profile.to_path_buf()));
        }

        let port = free_loopback_port()?;
        let mut args: Vec<String> = vec![
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", profile.display()),
            "--remote-debugging-address=127.0.0.1".to_owned(),
            "--window-size=1280,900".to_owned(),
        ];
        if config.headless {
            args.push("--headless=new".to_owned());
        }
        args.extend(SAFETY_ARGS.iter().map(|a| (*a).to_owned()));
        args.extend(extra_args.iter().cloned());

        let spec = LaunchSpec {
            executable: executable.to_path_buf(),
            args,
            working_dir: Some(profile.to_path_buf()),
            env: browser_environment(executable),
            timeout: None,
            output_cap_bytes: 1024 * 1024,
            stdout_path: None,
            stderr_path: None,
            terminal: TerminalMode::Pipes,
        };
        let child = jean_process::piped::spawn(&spec).map_err(|e| BrowserError::Launch {
            reason: e.to_string(),
        })?;
        let control = child.control();
        let stderr = child.stderr_file().map_err(|e| BrowserError::Launch {
            reason: format!("its stderr could not be read: {e}"),
        })?;

        let (tx, rx) = mpsc::channel::<String>();
        let pump = std::thread::spawn(move || pump_stderr(stderr, tx));
        let ws_url = match read_endpoint(&rx, Duration::from_secs(config.startup_timeout_secs)) {
            Some(url) if endpoint_is_loopback(&url) => url,
            Some(url) => {
                let _ = control.terminate();
                finish_stderr(pump);
                return Err(BrowserError::Endpoint {
                    reason: format!("{url} is not a loopback debugging address"),
                });
            }
            None => {
                let _ = control.terminate();
                finish_stderr(pump);
                return Err(BrowserError::Endpoint {
                    reason: format!(
                        "the browser printed no debugging endpoint within {} s",
                        config.startup_timeout_secs
                    ),
                });
            }
        };

        // The client is attached, not the launcher: `connect` takes an endpoint and spawns nothing, and
        // it does not close a browser it did not start — which is why `close` below has to.
        let cdp = match CdpBrowser::connect(ws_url.clone()) {
            Ok(browser) => browser,
            Err(e) => {
                let _ = control.terminate();
                finish_stderr(pump);
                return Err(BrowserError::Endpoint {
                    reason: format!("the endpoint refused the CDP handshake: {e}"),
                });
            }
        };
        let action_timeout = Duration::from_secs(config.action_timeout_secs);
        cdp.set_default_timeout(action_timeout);
        Ok(Self {
            child: Some(child),
            control,
            cdp,
            tab: None,
            profile: profile.to_path_buf(),
            action_timeout,
            ws_url,
            stderr_pump: Some(pump),
        })
    }

    pub fn pid(&self) -> u32 {
        self.control.pid()
    }

    /// The endpoint J.E.A.N. dialled, for the record. Never secrets, but never implied either.
    pub fn endpoint(&self) -> &str {
        &self.ws_url
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile
    }

    /// Whether the browser process is still alive.
    pub fn is_running(&self) -> bool {
        self.control.is_running()
    }

    /// The one tab this crate drives, created on first use and reused after.
    fn tab(&mut self) -> Result<Arc<Tab>> {
        if let Some(tab) = &self.tab {
            return Ok(tab.clone());
        }
        let tab = self.cdp.new_tab()?;
        tab.set_default_timeout(self.action_timeout);
        self.tab = Some(tab.clone());
        Ok(tab)
    }

    /// Navigate, and wait for the load event the page is expected to fire.
    ///
    /// The origin check happens **here**, on the URL string, before CDP sees it: an allowlist enforced
    /// only by a listener inside the browser is a suggestion the browser can decline.
    pub fn navigate(&mut self, config: &BrowserConfig, url: &str) -> Result<()> {
        if !Origin::of(url)
            .as_ref()
            .is_some_and(|origin| origin.is_allowed_by(&config.allowed_origins))
        {
            return Err(BrowserError::OriginDenied {
                url: url.to_owned(),
                allowed: config.allowed_origins.join(", "),
            });
        }
        let tab = self.tab()?;
        tab.navigate_to(url)?;
        tab.wait_until_navigated()?;
        Ok(())
    }

    /// The document's own title.
    pub fn title(&mut self) -> Result<String> {
        Ok(self.tab()?.get_title()?)
    }

    /// The visible text of one element — what a person reading the page would say it says.
    pub fn text_of(&mut self, selector: &str) -> Result<String> {
        let tab = self.tab()?;
        let element = tab
            .wait_for_element(selector)
            .map_err(|e| BrowserError::Action {
                reason: format!("{selector} never appeared: {e}"),
            })?;
        element.get_inner_text().map_err(|e| BrowserError::Action {
            reason: format!("{selector} has no readable text: {e}"),
        })
    }

    /// The whole page, capped: a page may not decide how much of this process's memory it uses.
    ///
    /// Returns the retained text and whether more existed.
    pub fn content(&mut self, max_bytes: usize) -> Result<(String, bool)> {
        let html = self.tab()?.get_content()?;
        let truncated = html.len() > max_bytes;
        let mut kept = if truncated {
            let mut cut = max_bytes;
            while cut > 0 && !html.is_char_boundary(cut) {
                cut -= 1;
            }
            html[..cut].to_owned()
        } else {
            html
        };
        if truncated {
            kept.push_str("…[truncated]");
        }
        Ok((kept, truncated))
    }

    /// A real click: scroll it into view, move the pointer, press and release at its midpoint.
    ///
    /// Not `element.click()` in JavaScript. The workflow this crate exists to demonstrate is that a
    /// person's action changed the page, and a synthesized DOM event proves less than an input event
    /// the page had to respond to.
    pub fn click(&mut self, selector: &str) -> Result<()> {
        let tab = self.tab()?;
        let element = tab.wait_for_element(selector)?;
        element.click()?;
        Ok(())
    }

    /// Focus one element and type into it, as keystrokes rather than as a value assignment.
    pub fn fill(&mut self, selector: &str, text: &str) -> Result<()> {
        let tab = self.tab()?;
        let element = tab.wait_for_element(selector)?;
        element.type_into(text)?;
        Ok(())
    }

    /// A PNG of the page as it now renders, written to `path`, returning the byte count.
    ///
    /// This is the second witness: the DOM read and the rendered surface are produced by different
    /// parts of the browser, and a page that lied about its own text still has to draw the pixels.
    pub fn screenshot(&mut self, path: &Path) -> Result<u64> {
        let bytes = self.tab()?.capture_screenshot(
            Page::CaptureScreenshotFormatOption::Png,
            None,
            None,
            true,
        )?;
        let len = bytes.len() as u64;
        std::fs::write(path, bytes).map_err(|e| BrowserError::Action {
            reason: format!("the screenshot could not be written: {e}"),
        })?;
        Ok(len)
    }

    /// The URL the tab is actually showing, which is not necessarily the one that was asked for.
    pub fn current_url(&mut self) -> Result<String> {
        Ok(self.tab()?.get_url())
    }

    /// End the session: close the CDP connection, end the process tree, and report the exit code.
    ///
    /// `Browser.close` is not sent — the client was attached, and it is the polite option for a browser
    /// it does not own. The job object is what owns this one, and a job object has no graceful variant:
    /// closing it takes the browser, its renderers and its GPU process together, which is the guarantee
    /// the operator was given when the profile was made disposable.
    pub fn close(mut self) -> Result<Option<i32>> {
        let code = self.shutdown();
        self.child = None;
        Ok(code)
    }

    fn shutdown(&mut self) -> Option<i32> {
        // Drop the client first: it owns the websocket, and the reader thread on the other end of the
        // stderr pump has to see the process go away to finish.
        self.tab = None;
        let until = Instant::now() + self.action_timeout.min(Duration::from_secs(5));
        let _ = self.control.terminate();
        while !self.control.tree_empty().unwrap_or(false) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let code = self.control.exit_code_if_settled();
        if let Some(pump) = self.stderr_pump.take() {
            finish_stderr(pump);
        }
        code
    }
}

impl Drop for OwnedBrowser {
    fn drop(&mut self) {
        // The path a `?` takes on the way out. A browser left running because a page did not contain
        // what was expected is the exact failure this crate promises the operator it cannot have.
        if let Some(pump) = self.stderr_pump.take() {
            // The main process can exit while descendants still own the pipe.
            let _ = self.control.terminate();
            finish_stderr(pump);
        }
    }
}

fn finish_stderr(pump: JoinHandle<()>) {
    let until = Instant::now() + Duration::from_secs(1);
    while !pump.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    if pump.is_finished() {
        let _ = pump.join();
    }
}

/// The port Chrome will listen on, chosen by the OS and then let go of again.
///
/// Chrome refuses a port already in use, and a fixed guess is a collision waiting for the next
/// application on this machine. There is a window between binding and releasing and handing the number
/// to Chrome — the same window `headless_chrome` lives in — and a browser that fails to bind says so on
/// stderr, which the pump below reports as its own error rather than as a timeout.
fn free_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| BrowserError::Launch {
        reason: format!("no loopback port could be reserved: {e}"),
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| BrowserError::Launch {
            reason: format!("the reserved port is unknown: {e}"),
        })?
        .port();
    drop(listener);
    Ok(port)
}

/// The environment the browser child gets.
///
/// Deliberately not inherited: J.E.A.N. runs a browser, not the operator's shell, and a child that sees
/// a credential the configuration never mentioned is a leak with a pid.
fn browser_environment(executable: &Path) -> std::collections::BTreeMap<String, String> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"));
    let system = Path::new(&root).join("System32");
    let mut env = std::collections::BTreeMap::new();
    env.insert("SystemRoot".to_owned(), root.clone());
    env.insert(
        "PATH".to_owned(),
        match executable.parent() {
            Some(dir) => format!("{};{};{}", system.display(), root, dir.display()),
            None => format!("{};{}", system.display(), root),
        },
    );
    env.insert(
        "COMSPEC".to_owned(),
        system.join("cmd.exe").to_string_lossy().into_owned(),
    );
    env.insert(
        "TEMP".to_owned(),
        std::env::var("TEMP").unwrap_or_else(|_| root.clone()),
    );
    env.insert(
        "TMP".to_owned(),
        std::env::var("TMP").unwrap_or_else(|_| root.clone()),
    );
    env.insert("LANG".to_owned(), "C.UTF-8".to_owned());
    env
}

/// Read stderr until the endpoint line appears, then keep draining without forwarding.
///
/// Draining is not optional: the pipe is 64 KiB, and a browser that writes diagnostics nobody reads
/// blocks on its next write and looks exactly like a browser that stopped working.
fn pump_stderr(mut stderr: std::fs::File, tx: mpsc::Sender<String>) {
    let mut buf = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    let mut announced = false;
    loop {
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                for byte in &buf[..n] {
                    if *byte == b'\n' {
                        let raw = std::mem::take(&mut line);
                        let text = String::from_utf8_lossy(&raw).into_owned();
                        if !text.trim().is_empty() {
                            if !announced {
                                if let Some(url) = endpoint_in(&text) {
                                    announced = true;
                                    if tx.send(url).is_err() {
                                        return;
                                    }
                                }
                            }
                            tracing::debug!(line = text.trim(), "browser stderr");
                        }
                    } else {
                        line.push(*byte);
                        if line.len() > 4096 {
                            line.clear();
                        }
                    }
                }
            }
        }
    }
}

/// The `ws://…/devtools/browser/…` URL inside one stderr line, if this is that line.
fn endpoint_in(line: &str) -> Option<String> {
    let at = line.find("listening on ")?;
    let rest = &line[at + "listening on ".len()..];
    let url = rest.split_whitespace().next()?;
    (url.starts_with("ws://") || url.starts_with("wss://")).then(|| url.to_owned())
}

/// One bounded wait for the endpoint. The pump only forwards lines that already parse as one, so a
/// timeout and a closed stderr are the same answer here: the browser did not say where it was listening.
fn read_endpoint(rx: &mpsc::Receiver<String>, budget: Duration) -> Option<String> {
    rx.recv_timeout(budget).ok()
}

/// The browsers this project knows how to find, in the order the decision lists them.
pub const KNOWN_BROWSERS: &[&str] = &[
    r"Google\Chrome\Application\chrome.exe",
    r"Microsoft\Edge\Application\msedge.exe",
    r"BraveSoftware\Brave-Browser\Application\brave.exe",
];

/// Where an installed browser lives on this machine, or `None`.
///
/// Reads the standard installation directories only. Finding nothing is reported by the caller as a
/// blocker; nothing here downloads or installs a browser, which is the difference between a driver and
/// an installer.
pub fn detect_browser() -> Option<PathBuf> {
    KNOWN_BROWSERS.iter().find_map(|relative| {
        for key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            let root = std::env::var(key).ok()?;
            let candidate = Path::new(&root).join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    })
}

/// Whether `url` may be opened under `allowed` — the check [`OwnedBrowser::navigate`] runs, exposed so a
/// caller can refuse before it starts a browser, a task row, or an audit line.
pub fn origin_permitted(config: &BrowserConfig, url: &str) -> bool {
    Origin::of(url).is_some_and(|origin| origin.is_allowed_by(&config.allowed_origins))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(origins: &[&str]) -> BrowserConfig {
        BrowserConfig {
            enabled: true,
            browser_path: String::new(),
            headless: true,
            startup_timeout_secs: 5,
            action_timeout_secs: 5,
            max_page_bytes: 4096,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn origin(url: &str) -> Origin {
        Origin::of(url).unwrap_or_else(|| panic!("{url} has no origin to read"))
    }

    #[test]
    fn an_origin_is_its_scheme_host_and_port_and_nothing_else() {
        // The path, the query and the fragment must not reach the allowlist comparison: a token in a
        // query string is the reason `allowed_origins` names origins rather than URLs.
        let page = origin("https://example.com:8443/a/b?token=secret#frag");
        assert_eq!(page.scheme, "https");
        assert_eq!(page.host, "example.com");
        assert_eq!(page.port, Some(8443));

        let plain = origin("http://127.0.0.1:9/open");
        assert_eq!(
            (plain.scheme.as_str(), plain.host.as_str()),
            ("http", "127.0.0.1")
        );
        assert_eq!(plain.port, Some(9));

        // A `file:` URL has no authority at all — and that is what the bare `file` entry is for.
        let local = origin("file:///C:/Users/jean/fixture.html");
        assert_eq!(local.scheme, "file");
        assert!(local.host.is_empty(), "host was {:?}", local.host);
        assert_eq!(local.port, None);

        assert!(
            Origin::of(r"C:\Users\jean\fixture.html").is_none(),
            "no scheme, no origin"
        );
    }

    #[test]
    fn a_listed_host_without_a_port_covers_any_port_on_that_host() {
        let allowed = config_with(&["file", "http://127.0.0.1"]);
        assert!(origin_permitted(&allowed, "file:///C:/any/where.html"));
        assert!(origin_permitted(&allowed, "http://127.0.0.1:43100/fixture"));
        assert!(
            origin_permitted(&allowed, "http://127.0.0.1/fixture"),
            "and the default port"
        );

        // The scheme is part of the match, not a decoration: `https` is a different promise.
        assert!(!origin_permitted(&allowed, "https://127.0.0.1/fixture"));
        assert!(!origin_permitted(&allowed, "http://example.com/"));
        assert!(
            !origin_permitted(&allowed, "http://10.0.0.1/x"),
            "loopback means loopback"
        );
        // A bare `file` entry binds the scheme, and a `file:` URL's host is not a destination.
        assert!(origin_permitted(&allowed, "file://localhost/C:/x.html"));
    }

    #[test]
    fn a_pinned_port_does_not_open_the_host() {
        let allowed = config_with(&["http://127.0.0.1:8080"]);
        assert!(origin_permitted(&allowed, "http://127.0.0.1:8080/x"));
        assert!(
            !origin_permitted(&allowed, "http://127.0.0.1:9999/x"),
            "an operator who pinned a port meant that port"
        );
    }

    #[test]
    fn only_a_loopback_endpoint_is_dialled() {
        // The URL Chrome prints is the one this process connects to, with total control on the other
        // end. Anything that is not loopback is refused by shape, not by preference.
        assert!(endpoint_is_loopback(
            "ws://127.0.0.1:9222/devtools/browser/abc"
        ));
        assert!(endpoint_is_loopback(
            "ws://localhost:9222/devtools/browser/abc"
        ));
        assert!(endpoint_is_loopback("ws://[::1]:9222/devtools/browser/abc"));
        assert!(!endpoint_is_loopback(
            "ws://10.0.0.5:9222/devtools/browser/abc"
        ));
        assert!(!endpoint_is_loopback(
            "ws://attacker.example/devtools/browser/abc"
        ));
        // `http://` would be a different protocol; `wss://` would be somebody else's certificate.
        assert!(!endpoint_is_loopback(
            "http://127.0.0.1:9222/devtools/browser/abc"
        ));
        assert!(!endpoint_is_loopback(""));
    }

    #[test]
    fn the_endpoint_is_read_from_the_line_chrome_actually_prints() {
        let line = "DevTools listening on ws://127.0.0.1:65123/devtools/browser/6a1c-4d2e";
        assert_eq!(
            endpoint_in(line).as_deref(),
            Some("ws://127.0.0.1:65123/devtools/browser/6a1c-4d2e")
        );
        assert_eq!(
            endpoint_in("ERROR:bind() failed"),
            None,
            "an error is not an endpoint"
        );
        assert_eq!(endpoint_in("[1234:5678] some other diagnostic"), None);
    }

    #[test]
    fn a_missing_executable_is_refused_before_anything_is_spawned() {
        let dir = std::env::temp_dir().join(format!("jean-browser-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("an empty profile directory");
        let error = OwnedBrowser::start(&config_with(&["file"]), &dir.join("nope.exe"), &dir, &[])
            .err()
            .expect("a path that does not exist is not an executable");
        assert!(
            matches!(error, BrowserError::MissingExecutable(_)),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_profile_that_already_holds_something_is_refused() {
        // This is the gate that makes "never the operator's browser" a property of the code rather than
        // a promise: J.E.A.N. starts on an empty profile it made, or it does not start.
        let dir = std::env::temp_dir().join(format!("jean-browser-used-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a profile directory");
        std::fs::write(dir.join("Preferences"), b"{}").expect("something already in it");

        let fake = std::env::current_exe().expect("this test's own executable");
        let error = OwnedBrowser::start(&config_with(&["file"]), &fake, &dir, &[])
            .err()
            .expect("a profile that already holds something must be refused");
        assert!(matches!(error, BrowserError::ProfileNotEmpty(_)), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
