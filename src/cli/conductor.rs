//! `aoe conductor` (alias `aoe ao`) command implementations. Gated on
//! `AOE_EXPERIMENTAL_AO_MODE`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use tokio::process::Command;
use tokio::signal;
use tokio::sync::oneshot;

use crate::conductor::executor::Executor;
use crate::conductor::github::{fetch_issues, session_title_for_issue};
use crate::conductor::intelligence::{Backoff, SessionPool};
use crate::conductor::policies::{ConductorPolicies, QuietHours};
use crate::conductor::reasoner::claude_print::ClaudePrintReasoner;
use crate::conductor::watcher::{Watcher, DEFAULT_POLL_INTERVAL};
use crate::conductor::{self};
use crate::session::{Instance, Storage};

#[derive(Subcommand)]
pub enum ConductorCommands {
    /// Rank sessions by attention score and print the top of the queue.
    Status(ConductorStatusArgs),

    /// Watch the fleet and log recommendations from the reasoner. This
    /// command is read-only: recommendations are logged, not applied.
    /// Action dispatch lands in a later commit.
    Watch(ConductorWatchArgs),

    /// Spawn one session per open GitHub issue. Uses the `gh` CLI so the
    /// user's existing auth applies. Dry-runs by default; pass `--live`
    /// to actually create sessions.
    Spawn(ConductorSpawnArgs),
}

#[derive(Args)]
pub struct ConductorStatusArgs {
    /// Output as JSON (id, title, score, status, one row per session)
    #[arg(long)]
    pub json: bool,

    /// Maximum number of rows to print. Defaults to 20.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Args)]
pub struct ConductorWatchArgs {
    /// Seconds between reasoner calls. Minimum is enforced by the
    /// watcher so scripted `--poll-interval 0` cannot burn subprocess
    /// spawns in a tight loop.
    #[arg(long, default_value_t = DEFAULT_POLL_INTERVAL.as_secs())]
    pub poll_interval: u64,

    /// Run one tick and exit. Handy for e2e / CI runs and for verifying
    /// the reasoner binary is reachable without committing to a loop.
    #[arg(long)]
    pub once: bool,

    /// Path to the `claude` binary. Defaults to `claude` on PATH; override
    /// for a local build or a subprocess shim in tests.
    #[arg(long)]
    pub reasoner_binary: Option<String>,

    /// Actually apply recommended actions to session state. Off by default
    /// so a first-time run is safe: recommendations are logged, not
    /// executed. `Nudge` and `Archive` remain blocked in live mode unless
    /// their opt-in flags are also set.
    #[arg(long)]
    pub live: bool,

    /// Opt in to `Nudge` actions (injecting a message into a running
    /// session's agent). No effect without `--live`.
    #[arg(long)]
    pub allow_nudge: bool,

    /// Opt in to `Archive` actions (moving a session out of the active
    /// view). No effect without `--live`.
    #[arg(long)]
    pub allow_destructive: bool,

    /// Minimum seconds between successive actions on the same session.
    /// Ports aoaoe's `actionCooldownMs`.
    #[arg(long, default_value_t = 30)]
    pub action_cooldown_secs: u64,

    /// Skip reasoning during this daily window, `HH:MM-HH:MM` in the
    /// daemon's local timezone. Wraps around midnight. Ports aoaoe's
    /// `quietHours`.
    #[arg(long)]
    pub quiet_hours: Option<String>,
}

#[derive(Args)]
pub struct ConductorSpawnArgs {
    /// GitHub repository in `owner/repo` form.
    #[arg(long)]
    pub repo: String,

    /// Issue state filter passed to `gh issue list --state`. Defaults to
    /// `open`; `closed` or `all` are the other useful values.
    #[arg(long, default_value = "open")]
    pub state: String,

    /// Maximum number of issues to spawn. Chosen conservatively; blowing
    /// through 100 sessions with one command tends to be a mistake.
    #[arg(long, default_value_t = 5)]
    pub limit: u32,

    /// Optional single-label filter passed to `gh issue list --label`.
    #[arg(long)]
    pub label: Option<String>,

    /// Base project path each spawned session is rooted at. Defaults to
    /// the current directory.
    #[arg(long, default_value = ".")]
    pub path: PathBuf,

    /// Prefix prepended to each session title. Defaults to empty so the
    /// title reads `#42 the issue title`.
    #[arg(long, default_value = "")]
    pub title_prefix: String,

    /// Actually create sessions. Off by default: a first run prints the
    /// preview so users see exactly what will happen. Once
    /// `AOE_EXPERIMENTAL_AO_MODE=1` and `--live` are both set, spawning
    /// begins.
    #[arg(long)]
    pub live: bool,

    /// Maximum number of concurrently active sessions across the profile
    /// (Running / Waiting / Idle). Once the cap is reached, additional
    /// issues in the input list are skipped rather than queued. Matches
    /// aoaoe's `session-pool` cap. See `SessionPool`.
    #[arg(long, default_value_t = 20)]
    pub max_active: usize,
}

#[derive(Serialize)]
struct ScoredJson {
    id: String,
    title: String,
    score: i64,
    status: String,
    project_path: String,
}

#[tracing::instrument(target = "cli.conductor", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, command: ConductorCommands) -> Result<()> {
    conductor::require_enabled()?;
    match command {
        ConductorCommands::Status(args) => status(profile, args).await,
        ConductorCommands::Watch(args) => watch(profile, args).await,
        ConductorCommands::Spawn(args) => spawn(profile, args).await,
    }
}

async fn spawn(profile: &str, args: ConductorSpawnArgs) -> Result<()> {
    let issues = fetch_issues(&args.repo, &args.state, args.limit, args.label.as_deref()).await?;
    if issues.is_empty() {
        println!(
            "No {} issues in {} matching the filter.",
            args.state, args.repo
        );
        return Ok(());
    }

    println!("Fetched {} issue(s) from {}:", issues.len(), args.repo);
    for issue in &issues {
        println!("  {}", session_title_for_issue(&args.title_prefix, issue));
    }

    // Respect the pool cap even in dry-run so the preview reflects what
    // `--live` would actually do. The cap is intentionally generous; a
    // user wanting to blow past it can raise `--max-active`.
    let pool = SessionPool::new(args.max_active);
    let storage = Storage::new_unwatched(profile)?;
    let (fleet, _) = storage.load_with_groups()?;
    let slots = pool.slots_remaining(&fleet);
    let clipped: Vec<_> = issues.iter().take(slots).collect();
    let dropped = issues.len().saturating_sub(clipped.len());
    if dropped > 0 {
        println!(
            "\nSession pool full: {} active session(s) already, cap is {}. Skipping {} issue(s).",
            fleet.iter().filter(|i| is_active(i)).count(),
            args.max_active,
            dropped
        );
    }

    if !args.live {
        println!(
            "\nDry run. Rerun with `--live` to create {} session(s).",
            clipped.len()
        );
        return Ok(());
    }

    let self_exe = std::env::current_exe().context("resolve current aoe binary")?;
    let backoff = Backoff::new(
        std::time::Duration::from_millis(500),
        std::time::Duration::from_secs(5),
    )
    .with_jitter_bp(2000);
    let mut successes = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for issue in clipped {
        let title = session_title_for_issue(&args.title_prefix, issue);
        match spawn_one_with_retry(profile, &self_exe, &args.path, &title, &backoff).await {
            Ok(()) => {
                successes += 1;
                println!("  created  {}", title);
            }
            Err(err) => {
                failures.push(format!("#{}: {}", issue.number, err));
                println!("  failed   #{}  {}", issue.number, err);
            }
        }
    }
    println!(
        "\nCreated {} session(s), {} failure(s).",
        successes,
        failures.len()
    );
    if !failures.is_empty() {
        anyhow::bail!("some spawns failed: {}", failures.join("; "));
    }
    Ok(())
}

fn is_active(inst: &Instance) -> bool {
    if inst.archived_at.is_some() || inst.trashed_at.is_some() {
        return false;
    }
    matches!(
        inst.status,
        crate::session::Status::Running
            | crate::session::Status::Waiting
            | crate::session::Status::Idle
    )
}

async fn spawn_one_with_retry(
    profile: &str,
    self_exe: &std::path::Path,
    path: &std::path::Path,
    title: &str,
    backoff: &Backoff,
) -> Result<()> {
    let max_attempts = 3;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..max_attempts {
        match spawn_one(profile, self_exe, path, title).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < max_attempts {
                    let delay = backoff.delay(attempt);
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "spawn_one failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

async fn spawn_one(
    profile: &str,
    self_exe: &std::path::Path,
    path: &std::path::Path,
    title: &str,
) -> Result<()> {
    let output = Command::new(self_exe)
        .arg("--profile")
        .arg(profile)
        .arg("add")
        .arg(path)
        .arg("--title")
        .arg(title)
        .output()
        .await
        .context("spawn aoe add subprocess")?;
    if !output.status.success() {
        anyhow::bail!(
            "aoe add exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn watch(profile: &str, args: ConductorWatchArgs) -> Result<()> {
    let quiet_hours = match args.quiet_hours.as_deref() {
        Some(spec) => Some(QuietHours::parse(spec).context("--quiet-hours")?),
        None => None,
    };
    let policies = ConductorPolicies {
        allow_destructive: args.allow_destructive,
        allow_nudge: args.allow_nudge,
        action_cooldown: Duration::from_secs(args.action_cooldown_secs),
        quiet_hours,
    };

    let reasoner = match args.reasoner_binary {
        Some(bin) => ClaudePrintReasoner::with_binary(bin),
        None => ClaudePrintReasoner::default(),
    };
    let mut watcher = Watcher::new(
        profile.to_string(),
        reasoner,
        Duration::from_secs(args.poll_interval),
    );
    if let Some(window) = policies.quiet_hours {
        watcher = watcher.with_quiet_hours(window);
    }
    if args.live {
        watcher = watcher.with_executor(Executor::new(profile.to_string(), policies.clone()));
    }

    if args.once {
        let recs = watcher.tick().await?;
        if args.live {
            let outcomes = Executor::new(profile.to_string(), policies).dispatch(&recs)?;
            println!("{}", serde_json::to_string_pretty(&outcomes)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&recs)?);
        }
        return Ok(());
    }

    let (tx, rx) = oneshot::channel();
    let shutdown_task = tokio::spawn(async move {
        let _ = signal::ctrl_c().await;
        let _ = tx.send(());
    });
    let result = watcher.run(rx).await;
    shutdown_task.abort();
    result
}

async fn status(profile: &str, args: ConductorStatusArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;

    if instances.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("No sessions in profile '{}'.", storage.profile());
        }
        return Ok(());
    }

    crate::tmux::refresh_session_cache();
    for inst in &mut instances {
        inst.update_status();
    }

    let mut scored: Vec<(i64, Instance)> = instances
        .into_iter()
        .filter_map(|inst| conductor::attention_score(&inst).map(|s| (s, inst)))
        .collect();
    scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    scored.truncate(args.limit);

    if args.json {
        let out: Vec<ScoredJson> = scored
            .into_iter()
            .map(|(score, inst)| ScoredJson {
                id: inst.id,
                title: inst.title,
                score,
                status: format!("{:?}", inst.status),
                project_path: inst.project_path,
            })
            .collect();
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!("{:>5}  {:<14}  {:<24}  PATH", "SCORE", "STATUS", "TITLE");
        for (score, inst) in scored {
            let title = super::truncate(&inst.title, 24);
            let path = super::truncate(&inst.project_path, 40);
            println!(
                "{:>5}  {:<14}  {:<24}  {}",
                score,
                format!("{:?}", inst.status),
                title,
                path,
            );
        }
    }

    Ok(())
}
