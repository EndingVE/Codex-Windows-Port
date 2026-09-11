//! Re-authenticating a dead session by launching **the CLI that owns it**.
//!
//! Codex and Claude store their credentials in their own files/auth stores; this
//! port only ever *reads* them. When a session dies (`refresh_token_reused → log
//! out and sign in again`, `OAuth session expired and could not be refreshed`)
//! the port cannot refresh it itself — the original macOS app does not either.
//! What the original does, and what this module does, is hand the job back to the
//! owning CLI:
//!
//! | provider | command |
//! | --- | --- |
//! | codex | `codex login` |
//! | claude | `claude` |
//!
//! The child is started with a **new, visible console** and its stdio is left
//! alone: the CLI owns the interactive prompt and writes its own credentials.
//! This module never captures output, never parses it, and never writes a
//! credential itself — there is nothing here that could leak a secret because
//! nothing here reads one.
//!
//! State is a small machine the UI can poll:
//!
//! ```text
//! start() ─▶ "launched" ─▶ "running" ─┬─▶ "finished" (exit code, success)
//!                                     ├─▶ "finished" (timed out → killed)
//!                                     └─▶ "finished" (cancelled → killed)
//! ```
//!
//! A re-auth has a timeout so an abandoned console cannot stay "running" forever,
//! and can be cancelled from the UI.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use codexbar_core::ProviderId;
use serde::{Deserialize, Serialize};

/// Default time budget for an interactive sign-in.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// Windows `CREATE_NEW_CONSOLE` — the flag that makes the CLI's own window
/// appear. The port never reads the child's console.
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// One launchable sign-in command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSpec {
    /// Executable to run (`cmd` on Windows, so a `.cmd` shim also works).
    pub program: String,
    /// Arguments, verbatim.
    pub args: Vec<String>,
    /// Human description (`codex login`) for the UI and the log line.
    pub label: String,
}

/// The command that signs `id` back in, if this port knows one.
///
/// Only the two CLIs that own a dead session here are wired. Every other
/// provider signs in with an API key or a file, which is not a process launch.
pub fn command_for(id: ProviderId) -> Option<CommandSpec> {
    let label = match id {
        ProviderId::Codex => "codex login",
        ProviderId::Claude => "claude",
        // Copilot's device flow is driven in-process — see `login.rs`.
        _ => return None,
    };
    Some(wrapped(label))
}

/// True when [`command_for`] would return a command.
///
/// The settings page uses it to decide whether a session row gets a
/// Re-authenticate button.
pub fn can_launch(id: ProviderId) -> bool {
    command_for(id).is_some()
}

/// Turn a command line into a runnable [`CommandSpec`].
///
/// On Windows the command is run through `cmd /C` so a CLI installed as a batch
/// shim (the npm/global-install case) is found the same way a user's shell finds
/// it.
#[cfg(windows)]
fn wrapped(label: &str) -> CommandSpec {
    CommandSpec {
        program: "cmd".to_string(),
        args: vec!["/C".to_string(), label.to_string()],
        label: label.to_string(),
    }
}

#[cfg(not(windows))]
fn wrapped(label: &str) -> CommandSpec {
    let mut parts = label.split_whitespace();
    let program = parts.next().unwrap_or(label).to_string();
    CommandSpec {
        program,
        args: parts.map(str::to_string).collect(),
        label: label.to_string(),
    }
}

/// One status snapshot — safe to print, carries no credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReauthStatus {
    /// Provider id the launch is for (`codex`, `claude`), empty when idle.
    pub provider: String,
    /// Human title (`Codex`, `Claude`).
    pub title: String,
    /// `idle` | `launched` | `running` | `finished`.
    pub state: String,
    /// Command line shown to the user (`codex login`).
    pub command: String,
    /// OS process id while a child exists.
    pub pid: Option<u32>,
    /// Time budget, seconds.
    pub timeout_secs: u64,
    /// Seconds since launch.
    pub elapsed_secs: u64,
    /// True when the timeout killed the child.
    pub timed_out: bool,
    /// True when the user cancelled it.
    pub cancelled: bool,
    /// Exit code, when the child exited on its own.
    pub exit_code: Option<i32>,
    /// True only for a clean `exit 0` before the timeout and without a cancel.
    pub success: bool,
    /// One line the UI can show.
    pub message: String,
}

impl ReauthStatus {
    fn idle() -> Self {
        Self {
            provider: String::new(),
            title: String::new(),
            state: "idle".to_string(),
            command: String::new(),
            pid: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            elapsed_secs: 0,
            timed_out: false,
            cancelled: false,
            exit_code: None,
            success: false,
            message: "No re-authentication has been started.".to_string(),
        }
    }
}

/// Result of polling a running child.
enum Poll {
    Alive,
    Done(Box<ReauthStatus>),
}

struct Running {
    provider: String,
    title: String,
    label: String,
    pid: u32,
    child: Child,
    started: Instant,
    timeout: Duration,
    timed_out: bool,
    cancelled: bool,
}

impl Running {
    fn snapshot(
        &self,
        state: &str,
        success: bool,
        exit_code: Option<i32>,
        message: String,
    ) -> ReauthStatus {
        ReauthStatus {
            provider: self.provider.clone(),
            title: self.title.clone(),
            state: state.to_string(),
            command: self.label.clone(),
            pid: Some(self.pid),
            timeout_secs: self.timeout.as_secs(),
            elapsed_secs: self.started.elapsed().as_secs(),
            timed_out: self.timed_out,
            cancelled: self.cancelled,
            exit_code,
            success,
            message,
        }
    }

    fn poll(&mut self) -> Poll {
        match self.child.try_wait() {
            Ok(Some(exit)) => {
                let code = exit.code();
                let success = exit.success() && !self.timed_out && !self.cancelled;
                let message = if success {
                    format!(
                        "{} finished. Sign-in credentials were written by the CLI itself — refresh to pick them up.",
                        self.label
                    )
                } else {
                    format!(
                        "{} exited with {exit} (nothing was changed by CodexBar).",
                        self.label
                    )
                };
                Poll::Done(Box::new(self.snapshot("finished", success, code, message)))
            }
            Ok(None) => {
                if self.started.elapsed() >= self.timeout {
                    self.timed_out = true;
                    let _ = self.child.kill();
                    let exit = self.child.wait().ok();
                    Poll::Done(Box::new(self.snapshot(
                        "finished",
                        false,
                        exit.and_then(|e| e.code()),
                        format!(
                            "{} timed out after {} s and was stopped.",
                            self.label,
                            self.timeout.as_secs()
                        ),
                    )))
                } else {
                    Poll::Alive
                }
            }
            Err(err) => {
                let message = format!("could not check {}: {err}", self.label);
                Poll::Done(Box::new(self.snapshot("finished", false, None, message)))
            }
        }
    }
}

/// The re-auth state machine. One launch at a time.
#[derive(Default)]
pub struct ReauthManager {
    running: Option<Running>,
    last: Option<ReauthStatus>,
}

impl ReauthManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Launch `id`'s owning CLI in a new, visible console.
    pub fn start(&mut self, id: ProviderId) -> Result<ReauthStatus, String> {
        let spec = command_for(id)
            .ok_or_else(|| format!("{} has no CLI sign-in this port can launch", id.title()))?;
        self.start_spec(
            id.as_str(),
            id.title(),
            spec,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            true,
        )
    }

    /// Launch an explicit command. Split out so tests can drive the machine with
    /// a fake executable and a short timeout.
    ///
    /// `new_console` is honoured on Windows only. In production it is `true`, so
    /// the user sees the CLI's own window; the child's stdio is never piped and
    /// never captured.
    pub fn start_spec(
        &mut self,
        provider: &str,
        title: &str,
        spec: CommandSpec,
        timeout: Duration,
        new_console: bool,
    ) -> Result<ReauthStatus, String> {
        if self.running.is_some() {
            return Err(
                "a re-authentication is already running — finish or cancel it first".to_string(),
            );
        }

        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        // Deliberately no `.stdout/.stderr/.stdin` calls: capturing or piping the
        // CLI's output is exactly what this module must not do. The child owns its
        // console and its credentials.
        #[cfg(windows)]
        if new_console {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NEW_CONSOLE);
        }
        #[cfg(not(windows))]
        let _ = new_console;

        let child = command
            .spawn()
            .map_err(|err| format!("could not launch \"{}\": {err}", spec.label))?;
        let pid = child.id();

        self.last = None;
        let running = Running {
            provider: provider.to_string(),
            title: title.to_string(),
            label: spec.label.clone(),
            pid,
            child,
            started: Instant::now(),
            timeout,
            timed_out: false,
            cancelled: false,
        };
        let status = running.snapshot(
            "launched",
            false,
            None,
            format!(
                "Launched \"{}\" in its own window (pid {pid}). Complete the sign-in there; CodexBar never sees the credentials.",
                spec.label
            ),
        );
        self.running = Some(running);
        Ok(status)
    }

    /// Current status, advancing the machine (poll the child, enforce timeout).
    pub fn status(&mut self) -> ReauthStatus {
        if let Some(mut running) = self.running.take() {
            match running.poll() {
                Poll::Alive => {
                    let status = running.snapshot(
                        "running",
                        false,
                        None,
                        format!(
                            "\"{}\" is still open — finish the sign-in in its window.",
                            running.label
                        ),
                    );
                    self.running = Some(running);
                    status
                }
                Poll::Done(status) => {
                    self.last = Some((*status).clone());
                    *status
                }
            }
        } else {
            self.last.clone().unwrap_or_else(ReauthStatus::idle)
        }
    }

    /// Stop a running sign-in. The CLI's window closes; nothing is written.
    pub fn cancel(&mut self) -> ReauthStatus {
        let Some(mut running) = self.running.take() else {
            return self.last.clone().unwrap_or_else(ReauthStatus::idle);
        };
        running.cancelled = true;
        let _ = running.child.kill();
        let exit = running.child.wait().ok();
        let status = running.snapshot(
            "finished",
            false,
            exit.and_then(|e| e.code()),
            format!("{} was cancelled; CodexBar changed nothing.", running.label),
        );
        self.last = Some(status.clone());
        status
    }
}

impl std::fmt::Debug for ReauthManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReauthManager")
            .field("running", &self.running.as_ref().map(|r| r.pid))
            .field("last", &self.last)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread::sleep;

    /// Write a fake executable — a `.cmd` script — and return a spec that runs
    /// it. Nothing here needs a real codex/claude install.
    fn fake_exe(tag: &str, body: &str) -> (PathBuf, CommandSpec) {
        let dir =
            std::env::temp_dir().join(format!("codexbar-reauth-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}.cmd"));
        std::fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
        let spec = CommandSpec {
            program: "cmd".to_string(),
            args: vec!["/C".to_string(), path.to_string_lossy().into_owned()],
            label: format!("fake {tag}"),
        };
        (path, spec)
    }

    fn poll_until_done(manager: &mut ReauthManager) -> ReauthStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = manager.status();
            if status.state == "finished" {
                return status;
            }
            assert!(Instant::now() < deadline, "fake process never finished");
            sleep(Duration::from_millis(50));
        }
    }

    /// The commands the port advertises are exactly the CLIs that own their
    /// sessions; everything else is not launchable.
    #[test]
    fn only_codex_and_claude_advertise_a_cli_login() {
        assert!(can_launch(ProviderId::Codex));
        assert!(can_launch(ProviderId::Claude));
        assert!(!can_launch(ProviderId::Groq));
        assert!(!can_launch(ProviderId::Copilot));
        let codex = command_for(ProviderId::Codex).unwrap();
        assert!(codex.label.contains("codex login"));
        let claude = command_for(ProviderId::Claude).unwrap();
        assert_eq!(claude.label, "claude");
    }

    /// An unknown provider is refused, not launched.
    #[test]
    fn an_unsupported_provider_is_refused() {
        let mut manager = ReauthManager::new();
        assert!(manager.start(ProviderId::OpenRouter).is_err());
        assert_ne!(manager.status().state, "running");
    }

    /// The happy path: a fake executable that exits 0 reaches "finished" with
    /// success, without the manager ever touching its output.
    #[test]
    fn a_clean_exit_finishes_successfully() {
        let (_path, spec) = fake_exe("ok", "exit /b 0");
        let mut manager = ReauthManager::new();
        let launched = manager
            .start_spec("codex", "Codex", spec, Duration::from_secs(10), false)
            .unwrap();
        assert_eq!(launched.state, "launched");
        assert!(launched.pid.is_some());

        let done = poll_until_done(&mut manager);
        assert_eq!(done.exit_code, Some(0));
        assert!(done.success);
        assert!(!done.timed_out && !done.cancelled);
        assert_ne!(manager.status().state, "running");
        // The status is queryable after it ends.
        assert_eq!(manager.status().state, "finished");
    }

    /// A failing CLI is reported honestly (exit code, success=false).
    #[test]
    fn a_nonzero_exit_is_reported_as_a_failure() {
        let (_path, spec) = fake_exe("fail", "exit /b 7");
        let mut manager = ReauthManager::new();
        manager
            .start_spec("claude", "Claude", spec, Duration::from_secs(10), false)
            .unwrap();
        let done = poll_until_done(&mut manager);
        assert_eq!(done.exit_code, Some(7));
        assert!(!done.success);
    }

    /// A slow CLI is "running" until cancelled, and the cancel is observable.
    #[test]
    fn a_running_login_can_be_cancelled() {
        let (_path, spec) = fake_exe("slow", "ping -n 30 127.0.0.1 >nul");
        let mut manager = ReauthManager::new();
        manager
            .start_spec("codex", "Codex", spec, Duration::from_secs(60), false)
            .unwrap();
        sleep(Duration::from_millis(300));
        assert_eq!(manager.status().state, "running");

        let cancelled = manager.cancel();
        assert_eq!(cancelled.state, "finished");
        assert!(cancelled.cancelled);
        assert!(!cancelled.success);
        assert_ne!(manager.status().state, "running");
    }

    /// The timeout stops an abandoned login instead of leaving it "running"
    /// forever.
    #[test]
    fn a_login_that_outlives_its_timeout_is_killed() {
        let (_path, spec) = fake_exe("timeout", "ping -n 30 127.0.0.1 >nul");
        let mut manager = ReauthManager::new();
        manager
            .start_spec("codex", "Codex", spec, Duration::from_millis(200), false)
            .unwrap();
        sleep(Duration::from_millis(400));
        let status = manager.status();
        assert_eq!(status.state, "finished");
        assert!(status.timed_out);
        assert!(!status.success);
        assert_ne!(manager.status().state, "running");
    }

    /// Only one launch at a time.
    #[test]
    fn a_second_launch_is_refused_while_one_is_running() {
        let (_path, spec) = fake_exe("single", "ping -n 30 127.0.0.1 >nul");
        let mut manager = ReauthManager::new();
        manager
            .start_spec(
                "codex",
                "Codex",
                spec.clone(),
                Duration::from_secs(60),
                false,
            )
            .unwrap();
        let err = manager
            .start_spec("claude", "Claude", spec, Duration::from_secs(60), false)
            .unwrap_err();
        assert!(err.contains("already running"));
        manager.cancel();
    }

    /// Nothing the module renders carries a credential — it has none to carry.
    #[test]
    fn statuses_never_carry_a_credential() {
        let (_path, spec) = fake_exe("redact", "exit /b 0");
        let mut manager = ReauthManager::new();
        let launched = manager
            .start_spec("codex", "Codex", spec, Duration::from_secs(10), false)
            .unwrap();
        let rendered = format!("{launched:?} {}", serde_json::to_string(&launched).unwrap());
        assert!(rendered.contains("codex") || rendered.contains("fake"));
        assert!(!rendered.to_ascii_lowercase().contains("token"));
        let _ = poll_until_done(&mut manager);
    }
}
