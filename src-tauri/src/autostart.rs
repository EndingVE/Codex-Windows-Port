//! Autostart via the per-user Run key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` is what Task Manager's
//! *Startup apps* tab lists, so the toggle is visible and reversible for the
//! user — unlike a Task Scheduler entry or a service.
//!
//! `reg.exe` is used instead of a registry crate on purpose: it ships with
//! Windows, keeps the crate graph unchanged, and is trivial to audit in the
//! evidence scripts. Every call is spawned with `CREATE_NO_WINDOW` so the tray
//! app never flashes a console.
//!
//! The floating widget needs no extra Run argument: `widget.rs` restores its
//! visibility, position and modes from `widget.json` on every start, so a
//! login start brings the widget back exactly as the user left it. A
//! hand-written entry may still append `--widget` to force it visible; the
//! reconcile pass below compares only the executable path, so such arguments
//! are preserved instead of being "repaired" away on every start.

use std::process::Command;

/// Per-user Run key.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// Value name we own. Deliberately stable: the uninstaller and the UI both look
/// it up by name.
pub const VALUE_NAME: &str = "CodexBar";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("reg.exe");
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.output()
}

/// Command line currently registered for [`VALUE_NAME`], if any.
pub fn registered_command() -> Option<String> {
    let out = reg(&["query", RUN_KEY, "/v", VALUE_NAME]).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Output shape:
    //     CodexBar    REG_SZ    "C:\...\codexbar-win.exe"
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(VALUE_NAME) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((_kind, value)) = rest.split_once("REG_SZ") else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    // Value exists but the type was unexpected (REG_EXPAND_SZ and friends).
    Some(String::new())
}

/// True when the Run entry exists (regardless of the path it points at).
pub fn is_enabled() -> bool {
    registered_command().is_some()
}

/// Absolute path of the running executable, for the Run value.
pub fn exe_path() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("could not resolve the executable path: {e}"))
}

/// Register (`enabled = true`) or remove (`false`) the Run entry.
///
/// Always rewrites the path when enabling so a moved/updated build fixes itself.
pub fn set_enabled(enabled: bool, exe: &str) -> Result<(), String> {
    if enabled {
        let value = format!("\"{exe}\"");
        let out = reg(&[
            "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
        ])
        .map_err(|e| format!("could not run reg.exe: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "reg add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    } else {
        let out = reg(&["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
            .map_err(|e| format!("could not run reg.exe: {e}"))?;
        // Deleting a value that does not exist exits non-zero; that is success
        // for our purposes ("make sure it is not there").
        if !out.status.success() && is_enabled() {
            return Err(format!(
                "reg delete failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

/// Make the registry agree with the persisted setting.
pub fn reconcile(desired: bool) {
    let exe = match exe_path() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("codexbar: autostart skipped — {err}");
            return;
        }
    };
    let current = registered_command();
    let matches = match (&current, desired) {
        (Some(value), true) => same_exe(&registered_exe(value), &exe),
        (Some(_), false) => false,
        (None, true) => false,
        (None, false) => true,
    };
    if matches {
        return;
    }
    if let Err(err) = set_enabled(desired, &exe) {
        eprintln!("codexbar: autostart could not be updated: {err}");
    }
}

/// Executable part of a Run value: the quoted path, or everything up to the
/// first space when unquoted. `"C:\a b\x.exe" --widget` → `C:\a b\x.exe`.
pub fn registered_exe(value: &str) -> String {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('"') {
        return rest.split('"').next().unwrap_or("").to_string();
    }
    // Unquoted: the whole value when it is an existing-looking path with
    // spaces and no switches, otherwise the first token.
    match value.find(" -") {
        Some(i) => value[..i].trim_end().to_string(),
        None => value.to_string(),
    }
}

/// Windows paths are case-insensitive.
fn same_exe(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_exe_handles_quotes_and_arguments() {
        assert_eq!(
            registered_exe(r#""C:\Program Files\CodexBar\codexbar-win.exe""#),
            r"C:\Program Files\CodexBar\codexbar-win.exe"
        );
        assert_eq!(
            registered_exe(r#""C:\Apps\codexbar-win.exe" --widget"#),
            r"C:\Apps\codexbar-win.exe"
        );
        assert_eq!(
            registered_exe(r"C:\Apps\codexbar-win.exe --widget"),
            r"C:\Apps\codexbar-win.exe"
        );
        assert_eq!(
            registered_exe(r"C:\My Apps\codexbar-win.exe"),
            r"C:\My Apps\codexbar-win.exe"
        );
        assert_eq!(registered_exe(""), "");
    }

    #[test]
    fn exe_comparison_ignores_case() {
        assert!(same_exe(
            r"C:\Apps\CodexBar-Win.exe",
            r"c:\apps\codexbar-win.exe"
        ));
        assert!(!same_exe(r"C:\Apps\a.exe", r"C:\Apps\b.exe"));
    }
}
