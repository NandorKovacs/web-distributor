use crate::{output, run};
use anyhow::{bail, ensure, Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn build(commit: &str, bundle: &Path) -> Result<()> {
    let bundle = bundle_path(bundle)?;
    let stage = tempfile::Builder::new()
        .prefix(".web-distributor-bundle-")
        .tempdir_in(
            bundle
                .parent()
                .context("expected a bundle parent directory")?,
        )
        .with_context(|| format!("stage bundle at {}", bundle.display()))?;
    let staged_bundle = stage.path().join("bundle");
    fs::create_dir(&staged_bundle).context("create staged bundle")?;

    let build_dir = tempfile::Builder::new()
        .prefix("web-distributor-build-")
        .tempdir()
        .context("create temporary build directory")?;
    let build_path = build_dir.path().canonicalize()?;
    let source = build_path.join("source");
    let target = build_path.join("target");
    let checkout = run(Command::new("git")
        .args(["--no-replace-objects", "worktree", "add", "--detach", "--"])
        .arg(&source)
        .arg(commit))
    .with_context(|| format!("check out Git commit {commit}"));
    // A failed post-checkout hook can leave a registered worktree behind.
    if checkout.is_err() && !source.join(".git").is_file() {
        return checkout;
    }

    let built = checkout.and_then(|()| build_bundle(&source, &target, &staged_bundle, commit));
    let removed = run(Command::new("git")
        .args(["worktree", "remove", "--force", "--"])
        .arg(&source))
    .context("remove temporary Git worktree");
    if let Err(error) = built {
        if let Err(cleanup) = removed {
            bail!("{error:#}. Worktree cleanup failed because {cleanup:#}");
        }
        return Err(error);
    }
    removed?;

    // Staging beside the destination makes publication a single rename. The
    // rename also protects a destination populated by a concurrent invocation.
    fs::rename(staged_bundle, &bundle)
        .with_context(|| format!("publish bundle to {}", bundle.display()))
}

fn bundle_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .context("expected a bundle directory name")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let path = parent
        .canonicalize()
        .with_context(|| format!("resolve bundle parent directory {}", parent.display()))?
        .join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && fs::read_dir(&path)?.next().is_none(),
            "bundle destination {} must be an empty directory or a new path",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect bundle path {}", path.display()))
        }
    }
    Ok(path)
}

fn build_bundle(source: &Path, target: &Path, bundle: &Path, commit: &str) -> Result<()> {
    let caddy = std::env::var_os("CADDY").unwrap_or_else(|| "caddy".into());
    let caddy = PathBuf::from(caddy);
    let caddy = if caddy.is_relative() && caddy.components().count() > 1 {
        std::env::current_dir()?.join(caddy)
    } else {
        caddy
    };
    let cargo = || {
        let mut command = Command::new("cargo");
        command
            .current_dir(source)
            .env("CARGO_TARGET_DIR", target)
            .env("CADDY", &caddy);
        command
    };
    run(cargo().args(["test", "--locked"]))?;
    run(cargo().args([
        "test",
        "--locked",
        "--test",
        "caddy_runtime",
        "--",
        "--ignored",
    ]))
    .context("run Caddy runtime tests with HTTP/2-capable curl")?;
    run(cargo().args(["build", "--locked", "--release", "--bins"]))?;

    let release = target.join("release");
    for (binary, manifest) in [
        ("web-distributor", "cli.sha256"),
        ("web-distributor-migrate", "helper.sha256"),
    ] {
        run(Command::new("install")
            .args(["-m", "0755", "--"])
            .arg(release.join(binary))
            .arg(bundle.join(binary)))?;
        let digest = output(Command::new("sha256sum").current_dir(bundle).arg(binary))?;
        fs::write(bundle.join(manifest), format!("{digest}\n"))
            .with_context(|| format!("write {manifest}"))?;
    }
    fs::write(bundle.join("RELEASE"), format!("{commit}\n")).context("write RELEASE")?;
    run(Command::new(bundle.join("web-distributor")).arg("--version"))
}
