mod build;
mod guest;
mod smtp;
mod transport;

use anyhow::{bail, ensure, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use transport::Destination;

#[derive(Parser)]
#[command(about = "Build and operate web-distributor releases on cobalt")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Test and build an immutable local release bundle.
    Build {
        #[arg(default_value = "HEAD")]
        reference: String,
    },
    /// Install the built CLI and validate and activate its generated Caddy configuration.
    Deploy(Release),
    /// Restore the guest CLI and fragment saved by this deployment.
    Rollback(Release),
    /// Install the built SMTP certificate helper on cobalt and run the sync service.
    DeploySmtp(Release),
    /// Restore cobalt's previous SMTP helper and run the sync service.
    RollbackSmtp(Release),
    /// Check a public HTTPS route and its expected HTTP status from this workstation.
    Check {
        url: String,
        #[arg(value_parser = clap::value_parser!(u16).range(100..=599))]
        status: u16,
    },
    #[command(hide = true)]
    ApplyGuest { id: String, incoming: PathBuf },
    #[command(hide = true)]
    RestoreGuest { id: String },
    #[command(hide = true)]
    ApplySmtp { id: String, incoming: PathBuf },
    #[command(hide = true)]
    RestoreSmtp { id: String },
}

#[derive(Args)]
struct Release {
    /// Fetched commit or tag used for the build.
    #[arg(default_value = "HEAD")]
    reference: String,
    /// Use a fresh deployment ID for a subsequent installation of the same build.
    #[arg(long, default_value = "1")]
    attempt: NonZeroU32,
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(status.success(), "{command:?} exited with {status}");
    Ok(())
}

fn output(command: &mut Command) -> Result<String> {
    let result = command
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(
        result.status.success(),
        "{command:?} exited with {}",
        result.status
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn validate_id(id: &str) -> Result<&str> {
    let (commit, attempt) = id.split_once('-').map_or((id, None), |(c, a)| (c, Some(a)));
    ensure!(
        commit.len() == 40
            && commit
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "invalid release ID"
    );
    if let Some(a) = attempt {
        ensure!(a.parse::<NonZeroU32>()?.to_string() == a, "invalid attempt");
    }
    Ok(id)
}

fn resolve(reference: &str) -> Result<String> {
    let commit = output(Command::new("git").args([
        "--no-replace-objects",
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ]))?;
    validate_id(&commit)?;
    Ok(commit)
}

fn bundle(commit: &str) -> PathBuf {
    PathBuf::from("scratch/releases").join(commit)
}

impl Release {
    fn apply(self, dest: Destination, rollback: bool) -> Result<()> {
        let commit = resolve(&self.reference)?;
        let id = if self.attempt.get() == 1 {
            commit.clone()
        } else {
            format!("{commit}-{}", self.attempt)
        };
        let bundle = bundle(&commit);
        if !rollback {
            let stamp = fs::read_to_string(bundle.join("RELEASE")).with_context(|| {
                format!("missing release bundle; run ./ops/release.sh build {commit}")
            })?;
            ensure!(stamp.trim() == commit, "bundle does not match {commit}");
        }
        eprintln!(
            "{dest:?} {} for {id}",
            if rollback { "rollback" } else { "deployment" }
        );
        dest.execute(&id, (!rollback).then_some(bundle.as_path()))
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Action::Build { reference } => {
            let commit = resolve(&reference)?;
            let bundle = bundle(&commit);
            fs::create_dir_all(bundle.parent().unwrap())?;
            build::build(&commit, &bundle)?;
            println!("Built {commit} in {}", bundle.display());
            Ok(())
        }
        Action::Deploy(release) => release.apply(Destination::Guest, false),
        Action::Rollback(release) => release.apply(Destination::Guest, true),
        Action::DeploySmtp(release) => release.apply(Destination::Smtp, false),
        Action::RollbackSmtp(release) => release.apply(Destination::Smtp, true),
        Action::ApplyGuest { id, incoming } => guest::activate(validate_id(&id)?, &incoming),
        Action::RestoreGuest { id } => guest::rollback(validate_id(&id)?),
        Action::ApplySmtp { id, incoming } => smtp::activate(validate_id(&id)?, &incoming),
        Action::RestoreSmtp { id } => smtp::rollback(validate_id(&id)?),
        Action::Check { url, status } => {
            let got = output(Command::new("curl").args([
                "-q",
                "--globoff",
                "--proto",
                "=https",
                "--max-time",
                "20",
                "--silent",
                "--show-error",
                "--output",
                "/dev/null",
                "--write-out",
                "%{http_code}",
                "--",
                &url,
            ]))?;
            if got != status.to_string() {
                bail!("{url} returned HTTP {got}, expected {status}");
            }
            println!("{url} returned HTTP {status}");
            Ok(())
        }
    }
}
