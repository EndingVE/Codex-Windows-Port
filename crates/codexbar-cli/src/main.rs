//! `codexbar-win` — CLI entry point of the CodexBar Windows port.
//!
//! Two data sources:
//!
//! * **mock** (default) — `codexbar_core::mock`, deterministic sample data, no
//!   credentials, no network. Every fixture and screenshot in `evidence/` uses it.
//! * **live** (`--live`) — `codexbar_providers::live_registry()`, which reads local
//!   credentials and calls each provider's endpoints. Providers that are not
//!   implemented yet report `error` with a "pending provider worker" message
//!   rather than pretending to be unconfigured.
//!
//! The argument surface and the JSON contract in `codexbar-core` are identical in
//! both modes — that is the whole point of the frozen contract.

mod registry;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use codexbar_core::{ProviderId, UsageReport};

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard against a name collision that is easy to reintroduce and nasty to
    /// debug: the tray app's binary is `codexbar-win`, so if the CLI ever takes
    /// that name both crates write `target/debug/codexbar-win.exe` and the last
    /// build wins. A stale GUI binary then answers `usage --format json` by
    /// starting a tray app that never exits.
    #[test]
    fn cli_binary_name_is_stable() {
        assert_eq!(
            env!("CARGO_BIN_NAME"),
            "codexbar",
            "the CLI must be named `codexbar`; `codexbar-win` belongs to the tray app"
        );
    }

    #[test]
    fn every_provider_id_is_accepted_by_the_provider_flag() {
        for id in ProviderId::ALL {
            let parsed = parse_provider(id.as_str()).expect("canonical ids must parse");
            assert_eq!(parsed, id);
            // `--provider OpenRouter` works too: credentials are pasted by humans.
            assert_eq!(parse_provider(id.title()).unwrap(), id);
        }
        assert_eq!(ProviderId::ALL.len(), 14);
    }

    #[test]
    fn an_unknown_provider_lists_the_known_ones() {
        let err = parse_provider("nope").unwrap_err();
        assert!(err.contains("nope"));
        for id in ProviderId::ALL {
            assert!(
                err.contains(id.as_str()),
                "{err} must mention {}",
                id.as_str()
            );
        }
    }

    /// `--live` builds a registry for all 14 providers. No provider is fetched
    /// here, so the test stays offline.
    #[test]
    fn live_registry_covers_every_provider() {
        let registry = codexbar_providers::live_registry();
        let ids: Vec<ProviderId> = registry.iter().map(|p| p.id()).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "codexbar",
    version,
    about = "Usage limits for AI coding providers, on Windows.",
    long_about = "Reads quota windows (session / weekly) for every configured provider.\n\
                  Runs in MOCK mode by default (deterministic sample data); pass --live to\n\
                  read local credentials and fetch real usage from each provider."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the current usage snapshot for every provider.
    Usage(UsageArgs),
    /// List the providers this build knows about.
    Providers,
    /// Print the machine-readable payload schema version.
    Schema,
}

#[derive(Args, Debug)]
struct UsageArgs {
    /// Output format.
    #[arg(long, short, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Restrict output to a single provider (e.g. `openrouter`, `opencodego`).
    #[arg(long, short, value_name = "ID", value_parser = parse_provider)]
    provider: Option<ProviderId>,

    /// Serve deterministic mock data (the default). Kept explicit so scripts are
    /// already written the way the real build expects.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    mock: bool,

    /// Fetch real usage: read local credentials (env vars / the port's own
    /// config) and call each provider's usage endpoint. Wins over `--mock`.
    /// Credential *values* are never printed.
    #[arg(long)]
    live: bool,

    /// Exit with code 3 when any provider is at or above this used-percent.
    #[arg(long, value_name = "PERCENT")]
    fail_at: Option<f64>,
}

/// Parse `--provider` against the contract's own id list, so adding a provider
/// never needs an edit here.
fn parse_provider(raw: &str) -> Result<ProviderId, String> {
    ProviderId::from_str_lossy(raw).ok_or_else(|| {
        let known: Vec<&str> = ProviderId::ALL.iter().map(|p| p.as_str()).collect();
        format!(
            "unknown provider \"{raw}\"; known providers: {}",
            known.join(", ")
        )
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Human-readable table.
    Text,
    /// Machine-readable `UsageReport` JSON.
    Json,
    /// Single-line JSON (pipes, scripts, JQ).
    Jsonl,
    /// Compact TOON-style key/value output, mirroring `codexbar --format toon`.
    Toon,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("codexbar-win: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command.unwrap_or(Command::Usage(UsageArgs {
        format: Format::Text,
        provider: None,
        mock: true,
        live: false,
        fail_at: None,
    })) {
        Command::Providers => {
            for id in ProviderId::ALL {
                println!(
                    "{:<12} {:<14} {}",
                    id.as_str(),
                    id.auth_kind().as_str(),
                    id.title()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Schema => {
            let doc = serde_json::json!({
                "schemaVersion": codexbar_core::SCHEMA_VERSION,
                "providers": ProviderId::ALL.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
                "sample": serde_json::from_str::<serde_json::Value>(
                    &codexbar_core::mock_json(chrono::Utc::now())
                )?,
            });
            println!("{}", serde_json::to_string_pretty(&doc)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Usage(args) => {
            let live = args.live || !args.mock;
            let source = if live {
                DataSourceMode::Live
            } else {
                DataSourceMode::Mock
            };
            let mut report = match source {
                DataSourceMode::Mock => registry::current_report(),
                DataSourceMode::Live => registry::live_report(),
            };

            if let Some(wanted) = args.provider {
                report.providers.retain(|s| s.provider == wanted);
                if report.providers.is_empty() {
                    anyhow::bail!("no data for provider {wanted}");
                }
            }

            print_report(&report, args.format, source)?;

            let over = args
                .fail_at
                .map(|threshold| {
                    report
                        .providers
                        .iter()
                        .any(|p| p.max_used_percent() >= threshold)
                })
                .unwrap_or(false);

            Ok(if over {
                ExitCode::from(3)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

/// Which registry `usage` read from — drives the trailing provenance line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DataSourceMode {
    Mock,
    Live,
}

fn print_report(
    report: &UsageReport,
    format: Format,
    source: DataSourceMode,
) -> anyhow::Result<()> {
    match format {
        Format::Text => {
            print!("{report}");
            let worst = report
                .providers
                .iter()
                .filter_map(|p| p.headline_used_percent().map(|v| (v, p.title.clone())))
                .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((pct, title)) = worst {
                println!("\nmost constrained: {title} {pct:.0}% used");
            }
            match source {
                DataSourceMode::Mock => {
                    println!("source: mock (no credentials read, no network calls)")
                }
                DataSourceMode::Live => {
                    println!(
                        "source: live ({} of {} providers returned data; credentials read locally, no secrets printed)",
                        report
                            .providers
                            .iter()
                            .filter(|p| p.status == codexbar_core::FetchStatus::Ok)
                            .count(),
                        report.providers.len()
                    )
                }
            }
        }
        Format::Json => println!("{}", report.to_json_pretty()),
        Format::Jsonl => println!("{}", serde_json::to_string(report)?),
        Format::Toon => {
            println!("schemaVersion: {}", report.schema_version);
            println!("generatedAt: {}", report.generated_at.to_rfc3339());
            println!("generatedBy: {}", source.as_str());
            println!("providers[{}]:", report.providers.len());
            for p in &report.providers {
                println!("  - provider: {}", p.provider.as_str());
                println!("    title: {}", p.title);
                println!("    status: {}", p.status.as_str());
                println!("    source: {}", p.source.as_str());
                if let Some(r) = p.headline_used_percent() {
                    println!("    headlineUsedPercent: {r:.1}");
                }
                println!("    windows[{}]:", p.windows.len());
                for w in &p.windows {
                    println!("      - id: {}", w.id);
                    println!("        usedPercent: {:.1}", w.window.used_percent);
                    if let Some(t) = w.window.resets_at {
                        println!("        resetsAt: {}", t.to_rfc3339());
                    }
                }
            }
        }
    }
    Ok(())
}

impl DataSourceMode {
    const fn as_str(self) -> &'static str {
        match self {
            DataSourceMode::Mock => "mock",
            DataSourceMode::Live => "live",
        }
    }
}
