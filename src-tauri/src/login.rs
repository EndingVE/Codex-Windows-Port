//! Provider sign-in — the GitHub **device flow**, wired to the provider crate.
//!
//! One provider in the catalogue authenticates with a user-initiated device
//! code: GitHub Copilot (`AuthKind::DeviceFlow`). The flow lives with the
//! fetcher, and since it now exists as a public, fixture-tested API
//! (`codexbar_providers::providers::copilot::DeviceFlow`), this module drives it
//! for real instead of shipping a button that pretends to work:
//!
//! * [`start`] → `DeviceFlow::request_device_code()` — GitHub issues a
//!   `user_code` the user types on `verification_uri`. The **device code stays
//!   in this process** (it is a `Secret` and never enters a payload);
//! * [`poll`] → `DeviceFlow::poll_once()` — one poll per call, so the UI owns
//!   the waiting policy and a slow or denied login cannot block the tray;
//! * [`status`] → what this build can actually do for a provider, including the
//!   fact that a completed Copilot sign-in is now persisted.
//!
//! **Where the token goes.** The flow issues a GitHub OAuth token; this module
//! hands it to [`crate::token_store`], which writes it — atomically and without
//! ever printing it — into the port's own config at
//! `providers[id="copilot"].tokenAccounts`, the exact schema
//! `codexbar_providers::PortConfig::resolve_api_key` reads back. After a
//! successful authorize the next refresh therefore moves Copilot from
//! `notConfigured` to a real usage probe.
//!
//! Sign-in never happens while fetching: `fetch` stays read-only and never
//! starts the interactive flow. The token value never enters a payload the
//! webview can read, never a log line, and never an error message.
//! The settings page drives this with three commands:
//!
//! ```js
//! const started = await invoke("copilot_login_start");   // {userCode, verificationUri, …}
//! // show started.userCode, open started.verificationUri, then every intervalSecs:
//! const poll = await invoke("copilot_login_poll");        // {status: "pending" | …}
//! await invoke("copilot_login_cancel");                   // forget a flow the user abandoned
//! ```

use std::path::Path;

use codexbar_core::{AuthKind, ProviderId};
use codexbar_providers::providers::copilot::{DeviceCode, DeviceFlow, PollOutcome};
use serde::{Deserialize, Serialize};

/// File that exposes the flow this module drives.
pub const DEVICE_FLOW_ENTRY_POINT: &str = "crates/codexbar-providers/src/providers/copilot.rs";
/// The provider API this module calls.
pub const DEVICE_FLOW_METHODS: &str = "DeviceFlow::request_device_code() / DeviceFlow::poll_once()";
/// Where the issued token is persisted, atomically.
///
/// Kept as the `extension_point` of the honest failure branch: the flow can
/// authorize while the *store* fails (a config we cannot parse), and the UI
/// needs to name the seam.
pub const TOKEN_STORE_ENTRY_POINT: &str = "src-tauri/src/token_store.rs \
     (writes providers[id=\"copilot\"].tokenAccounts through codexbar_providers::write_json_atomic)";

/// What this build can do about signing a provider in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    /// Machine id (`copilot`, `codex`, …).
    pub provider: String,
    /// Display title, so a caller never has to map the id itself.
    pub title: String,
    /// `apiKey` | `localOAuthFile` | `deviceFlow` — the catalogue's own value.
    pub auth_kind: String,
    /// True only when a flow this app can start exists in this build.
    pub available: bool,
    /// `available` | `notAvailable` | `notApplicable`.
    pub status: String,
    /// Human explanation, safe to print (never carries a credential).
    pub message: String,
    /// Where the missing piece lives (empty when nothing is missing).
    pub extension_point: String,
    /// True when a completed flow can be persisted by this build.
    pub token_storage_available: bool,
    /// What the user can do *today*.
    pub instructions: String,
}

/// A started device flow. Carries the **user** code only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStarted {
    /// Always `pending` — the flow is waiting for the user in the browser.
    pub status: String,
    /// Short code the user types on the verification page (`ABCD-1234`).
    pub user_code: String,
    /// Page to open (pre-filled with the code when GitHub supplies one).
    pub verification_uri: String,
    /// How long the code lives, seconds.
    pub expires_in_secs: i64,
    /// How often to poll, seconds.
    pub interval_secs: i64,
    pub message: String,
}

/// One poll result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginPoll {
    /// `pending` | `slowDown` | `authorized` | `expired` | `denied` | `failed`.
    pub status: String,
    /// True only when the issued credential is persisted (never, yet — see the
    /// module docs).
    pub stored: bool,
    pub message: String,
    /// Present only while a step is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_point: Option<String>,
}

/// Sign-in story for one provider, as this build actually implements it.
pub fn status(id: ProviderId) -> LoginStatus {
    let mut out = LoginStatus {
        provider: id.as_str().to_string(),
        title: id.title().to_string(),
        auth_kind: id.auth_kind().as_str().to_string(),
        available: false,
        status: "notApplicable".to_string(),
        message: String::new(),
        extension_point: String::new(),
        token_storage_available: false,
        instructions: String::new(),
    };
    match id.auth_kind() {
        AuthKind::DeviceFlow => {
            out.available = true;
            out.status = "available".to_string();
            out.message = format!(
                "{} signs in with a GitHub device code; the flow is available here \
                 ({DEVICE_FLOW_METHODS} in {DEVICE_FLOW_ENTRY_POINT}) and the issued token is \
                 stored in the port's config.",
                id.title()
            );
            // The flow works end to end and the issued token is now persisted
            // (see `token_store.rs`).
            out.token_storage_available = true;
            out.instructions =
                "Start the sign-in from Settings, enter the code GitHub shows and approve \
                 it in the browser. CodexBar saves the token; it never prints it."
                    .to_string();
        }
        AuthKind::ApiKey => {
            out.instructions = format!(
                "Set {}'s API key in the environment or in \
                 %APPDATA%\\CodexBar\\config.json, then Refresh.",
                id.title()
            );
        }
        AuthKind::LocalOAuthFile => {
            out.instructions = format!(
                "Sign in with {}'s own CLI (it writes its credential file), then Refresh.",
                id.title()
            );
        }
    }
    out
}

/// Every provider's sign-in story, in `ProviderId::ALL` order.
pub fn catalog() -> Vec<LoginStatus> {
    ProviderId::ALL.iter().copied().map(status).collect()
}

/// True when at least one provider in this build has a startable flow.
pub fn any_flow_available() -> bool {
    ProviderId::ALL
        .iter()
        .copied()
        .any(|id| status(id).available)
}

// ---------------------------------------------------------------------------
// The flow itself
// ---------------------------------------------------------------------------

/// A started device flow: the payload the UI renders **plus** the in-process
/// handle the next poll needs (the device code never leaves this process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedFlow {
    pub payload: LoginStarted,
    pub device: DeviceCode,
}

/// Step 1: ask GitHub for a device code.
///
/// Returns only what the user needs to see in [`StartedFlow::payload`]; the
/// device code travels in [`StartedFlow::device`] and is never serialised.
pub fn start(flow: &DeviceFlow) -> Result<StartedFlow, String> {
    let code = flow
        .request_device_code()
        .map_err(|err| format!("Could not start the GitHub device flow: {err}"))?;
    let payload = LoginStarted {
        status: "pending".to_string(),
        user_code: code.user_code.clone(),
        verification_uri: code.verification_url().to_string(),
        expires_in_secs: code.expires_in,
        interval_secs: code.interval,
        message: format!(
            "Enter {} at {} and approve the sign-in, then keep this window open.",
            code.user_code,
            code.verification_url()
        ),
    };
    Ok(StartedFlow {
        payload,
        device: code,
    })
}

/// Step 2: one poll of a started flow. The caller owns the waiting policy.
///
/// `store_path` is the file a successfully issued token is persisted to
/// (`%APPDATA%\CodexBar\config.json` in production). Pass `None` to poll without
/// a store — the flow is still driven, and `stored: false` is reported honestly
/// because there was nowhere to keep the token.
pub fn poll(flow: &DeviceFlow, code: &DeviceCode, store_path: Option<&Path>) -> LoginPoll {
    let outcome = match flow.poll_once(&code.device_code) {
        Ok(outcome) => outcome,
        Err(err) => {
            return LoginPoll {
                status: "failed".to_string(),
                stored: false,
                message: format!("GitHub sign-in could not be checked: {err}"),
                extension_point: None,
            }
        }
    };

    let (status, message) = match outcome {
        PollOutcome::Pending => (
            "pending",
            "Waiting for GitHub — approve the sign-in in your browser.".to_string(),
        ),
        PollOutcome::SlowDown => (
            "slowDown",
            "GitHub asked for a slower poll; wait a moment before checking again.".to_string(),
        ),
        PollOutcome::Authorized(token) => {
            // The issued secret goes straight to the store. It is never printed,
            // never logged, and never put in the message — the message names the
            // file, not the token.
            return match store_path {
                Some(path) => match crate::token_store::store_device_token(
                    path,
                    ProviderId::Copilot.as_str(),
                    &token,
                    "GitHub device flow (Copilot)",
                ) {
                    Ok(stored) => LoginPoll {
                        status: "authorized".to_string(),
                        stored: true,
                        message: format!(
                            "Signed in. GitHub issued a token and it was saved to {}{} — \
                             Copilot will use it on the next refresh.",
                            stored.path,
                            if stored.replaced {
                                " (replacing the previous sign-in)"
                            } else {
                                ""
                            }
                        ),
                        extension_point: None,
                    },
                    Err(err) => LoginPoll {
                        status: "authorized".to_string(),
                        stored: false,
                        message: format!(
                            "GitHub approved the sign-in, but the token could not be saved: {err}"
                        ),
                        extension_point: Some(TOKEN_STORE_ENTRY_POINT.to_string()),
                    },
                },
                None => LoginPoll {
                    status: "authorized".to_string(),
                    stored: false,
                    message: "GitHub approved the sign-in, but no config path was given to keep \
                              the token, so Copilot will still report as not configured."
                        .to_string(),
                    extension_point: Some(TOKEN_STORE_ENTRY_POINT.to_string()),
                },
            };
        }
        PollOutcome::Expired => (
            "expired",
            "The GitHub code expired — start the sign-in again.".to_string(),
        ),
        PollOutcome::Denied => (
            "denied",
            "GitHub sign-in was denied. Nothing was changed.".to_string(),
        ),
        PollOutcome::Failed(message) => ("failed", message),
    };

    LoginPoll {
        status: status.to_string(),
        stored: false,
        message,
        extension_point: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexbar_providers::testing::{FixtureClient, FixtureResponse};
    use std::sync::Arc;

    /// Offline flow: an unreachable GitHub must produce an honest error, never a
    /// panic and never a fabricated code.
    fn offline_flow() -> DeviceFlow {
        DeviceFlow::with_client(Arc::new(FixtureClient::empty()), None)
    }

    /// The provider crate exposes the flow this module drives, and Copilot is
    /// the provider that advertises it.
    #[test]
    fn copilot_reports_the_flow_as_available_and_now_persistent() {
        let copilot = status(ProviderId::Copilot);
        assert_eq!(copilot.provider, "copilot");
        assert_eq!(copilot.auth_kind, "deviceFlow");
        assert!(copilot.available);
        assert_eq!(copilot.status, "available");
        assert!(copilot.message.contains("DeviceFlow::request_device_code"));
        // The issued token is now persisted (see `token_store.rs`).
        assert!(copilot.token_storage_available);
        assert!(any_flow_available());
    }

    /// API-key providers are not "login" flows at all — the honest answer is
    /// `notApplicable` plus the setup instruction.
    #[test]
    fn api_key_providers_are_not_applicable() {
        let groq = status(ProviderId::Groq);
        assert_eq!(groq.status, "notApplicable");
        assert!(!groq.available);
        assert!(groq.instructions.contains("config.json"));
        assert!(groq.extension_point.is_empty());
    }

    /// Every provider in the catalogue answers.
    #[test]
    fn catalog_covers_every_provider() {
        let all = catalog();
        assert_eq!(all.len(), ProviderId::ALL.len());
        for entry in &all {
            assert!(!entry.instructions.is_empty());
            if entry.auth_kind != "deviceFlow" {
                assert!(!entry.available);
            }
        }
    }

    /// A failed start is reported as an error and leaks nothing: the payload the
    /// UI would render carries no device code and no token.
    #[test]
    fn a_failed_start_is_an_honest_error_without_a_device_code() {
        let error = start(&offline_flow()).unwrap_err();
        assert!(error.contains("Could not start the GitHub device flow"));
        assert!(!error.contains("device_code"));
    }

    /// A poll against an unreachable GitHub reports `failed` with the provider's
    /// own message — it never claims the user is signed in.
    #[test]
    fn a_poll_that_cannot_reach_github_reports_failure() {
        let code = DeviceCode {
            device_code: codexbar_providers::Secret::new("«redacted:device-code»"),
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            verification_uri_complete: None,
            expires_in: 900,
            interval: 5,
        };
        let result = poll(&offline_flow(), &code, None);
        assert_eq!(result.status, "failed");
        assert!(!result.stored);
        assert!(!result.message.is_empty());
        // The device code never reaches the payload.
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains("device-code"));
    }

    /// A flow that returns a token writes it through the store, and the payload
    /// the webview reads says so without carrying the token.
    #[test]
    fn an_authorized_poll_persists_the_token() {
        const TOKEN: &str = "gho_fixture_device_flow_token";
        let dir = std::env::temp_dir().join(format!("codexbar-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let flow = DeviceFlow::with_client(
            Arc::new(FixtureClient::new(vec![FixtureResponse::json(
                200,
                r#"{"access_token":"gho_fixture_device_flow_token","token_type":"bearer","scope":"read:user"}"#,
            )])),
            None,
        );
        let code = DeviceCode {
            device_code: codexbar_providers::Secret::new("device-code-value"),
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            verification_uri_complete: None,
            expires_in: 900,
            interval: 5,
        };

        let result = poll(&flow, &code, Some(&path));
        assert_eq!(result.status, "authorized");
        assert!(result.stored, "{}", result.message);
        // The token never reaches the payload the UI reads.
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains(TOKEN));

        // …but the provider can now read it back.
        let env = codexbar_providers::Env::empty()
            .with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = codexbar_providers::PortConfig::load(&env).unwrap().unwrap();
        assert_eq!(
            config
                .resolve_api_key(ProviderId::Copilot, &env, &["COPILOT_API_TOKEN"])
                .unwrap()
                .expose(),
            TOKEN
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Authorizing without a store is reported honestly — `stored: false` and the
    /// extension point, never a fake success.
    #[test]
    fn an_authorized_poll_without_a_store_says_so() {
        let flow = DeviceFlow::with_client(
            Arc::new(FixtureClient::new(vec![FixtureResponse::json(
                200,
                r#"{"access_token":"gho_unsaved","token_type":"bearer"}"#,
            )])),
            None,
        );
        let code = DeviceCode {
            device_code: codexbar_providers::Secret::new("device-code-value"),
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            verification_uri_complete: None,
            expires_in: 900,
            interval: 5,
        };
        let result = poll(&flow, &code, None);
        assert_eq!(result.status, "authorized");
        assert!(!result.stored);
        assert!(result.extension_point.is_some());
        assert!(!result.message.contains("gho_unsaved"));
    }

    /// The started payload is serialisable and camelCase — that is what the
    /// settings page reads.
    #[test]
    fn started_payload_is_camel_case_and_carries_no_device_code() {
        let started = LoginStarted {
            status: "pending".to_string(),
            user_code: "ABCD-1234".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            expires_in_secs: 900,
            interval_secs: 5,
            message: "Enter ABCD-1234".to_string(),
        };
        let rendered = serde_json::to_string(&started).unwrap();
        assert!(rendered.contains("userCode"));
        assert!(rendered.contains("verificationUri"));
        assert!(!rendered.contains("deviceCode"));
        assert!(!rendered.contains("device_code"));
    }
}
