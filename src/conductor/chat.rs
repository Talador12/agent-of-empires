//! Minimal interactive REPL for the conductor. Ports the essence of
//! aoaoe's `chat.ts`: show the ranked queue, take a line of user context,
//! run one reasoner tick with that context folded into the prompt, print
//! the recommendations. No ratatui; the surface deliberately matches
//! what a scripted user would drive by hand.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result};

use super::observation::build_observation_with_signals;
use super::reasoner::claude_print::ClaudePrintReasoner;
use super::reasoner::{Reasoner, ReasonerMode};
use crate::session::Storage;

const HELP: &str =
    "Commands: <text> reason with hint, `:refresh` reload queue, `:q` quit, `:h` help.";

pub async fn run(profile: &str, mode: ReasonerMode) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("aoe conductor chat needs an interactive terminal");
    }
    let reasoner = ClaudePrintReasoner::for_mode(mode);
    println!("aoe conductor chat (profile: {profile})");
    println!("{HELP}");
    print_queue(profile).await?;

    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    loop {
        print!("conductor> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin_lock.read_line(&mut line)? == 0 {
            println!();
            return Ok(());
        }
        let trimmed = line.trim();
        match trimmed {
            "" => continue,
            ":q" | ":quit" | ":exit" => return Ok(()),
            ":h" | ":help" => {
                println!("{HELP}");
                continue;
            }
            ":refresh" => {
                print_queue(profile).await?;
                continue;
            }
            hint => {
                if let Err(err) = reason_once(profile, &reasoner, hint).await {
                    eprintln!("tick failed: {err}");
                }
            }
        }
    }
}

async fn print_queue(profile: &str) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    crate::tmux::refresh_session_cache();
    for inst in &mut instances {
        inst.update_status();
    }
    let observation = build_observation_with_signals(&instances, None);
    if observation.sessions.is_empty() {
        println!("(no sessions in this profile)");
        return Ok(());
    }
    println!("{:>5}  {:<12}  {:<20}  ID", "SCORE", "STATUS", "TITLE");
    for row in observation.sessions.iter().take(10) {
        let title = crate::cli::truncate(&row.title, 20);
        println!(
            "{:>5}  {:<12}  {:<20}  {}",
            row.attention_score,
            row.status,
            title,
            crate::cli::truncate_id(&row.id, 8)
        );
    }
    Ok(())
}

async fn reason_once(profile: &str, reasoner: &ClaudePrintReasoner, user_hint: &str) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    crate::tmux::refresh_session_cache();
    for inst in &mut instances {
        inst.update_status();
    }
    let observation = build_observation_with_signals(&instances, None);
    tracing::info!(hint = %user_hint, "chat tick");
    let recs = reasoner
        .recommend(&observation)
        .await
        .context("reasoner call failed")?;
    if recs.is_empty() {
        println!("(no recommendations)");
        return Ok(());
    }
    println!("{} recommendation(s):", recs.len());
    for r in recs {
        let confidence = r
            .confidence
            .map(|c| format!(" @ {c:.2}"))
            .unwrap_or_default();
        println!(
            "  session={} action={:?}{} rationale={}",
            r.session_id, r.action, confidence, r.rationale
        );
    }
    Ok(())
}
