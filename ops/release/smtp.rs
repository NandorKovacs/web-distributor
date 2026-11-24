use crate::run;
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN_DIR: &str = "/usr/local/bin";
const HELPER: &str = "web-distributor-migrate";
const SERVICE: &str = "sync-smtp-cert.service";

fn lock() -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(Path::new(BIN_DIR).join(".web-distributor-smtp.lock"))?;
    lock.try_lock_exclusive()
        .context("another SMTP helper deployment is running")?;
    Ok(lock)
}

fn state(id: &str) -> PathBuf {
    Path::new(BIN_DIR).join(format!(".web-distributor-smtp-{id}"))
}

fn replace(source: &Path) -> Result<()> {
    let temp = tempfile::tempdir_in(BIN_DIR)?;
    let new = temp.path().join(HELPER);
    fs::hard_link(source, &new)?;
    fs::rename(new, Path::new(BIN_DIR).join(HELPER))?;
    Ok(())
}

fn sync() -> Result<()> {
    let result = run(Command::new("systemctl").args(["start", SERVICE]));
    let _ = run(Command::new("journalctl").args(["-u", SERVICE, "-n", "20", "--no-pager"]));
    let _ =
        run(Command::new("systemctl").args(["list-timers", "sync-smtp-cert.timer", "--no-pager"]));
    result
}

pub fn activate(id: &str, incoming: &Path) -> Result<()> {
    let _lock = lock()?;
    let live = Path::new(BIN_DIR).join(HELPER);
    ensure!(
        live.symlink_metadata()?.is_file(),
        "expected a regular file at {}",
        live.display()
    );
    let backup = state(id);
    ensure!(
        !backup.try_exists()?,
        "SMTP deployment {id} exists; use a new --attempt"
    );
    let stage = tempfile::tempdir_in(BIN_DIR)?;
    let candidate = stage.path().join(HELPER);
    run(Command::new("install")
        .args(["-o", "root", "-g", "root", "-m", "0755"])
        .arg(incoming.join(HELPER))
        .arg(&candidate))?;
    // Verify the installed copy in root-owned staging before promoting it.
    let checksum = fs::read_to_string(incoming.join("helper.sha256"))?;
    let actual = crate::output(
        Command::new("sha256sum")
            .current_dir(stage.path())
            .arg(HELPER),
    )?;
    ensure!(checksum.trim_end() == actual, "wrong checksum for {HELPER}");
    run(Command::new(&candidate).args(["sync-smtp-tls", "--help"]))?;
    // Retaining both inodes distinguishes releases with identical helper bytes
    // and allows consecutive deployments to be rolled back in reverse order.
    fs::hard_link(&live, stage.path().join("previous"))?;
    let hashes = crate::output(
        Command::new("sha256sum")
            .current_dir(stage.path())
            .args([HELPER, "previous"]),
    )?;
    fs::write(stage.path().join("files.sha256"), format!("{hashes}\n"))?;
    fs::rename(stage.path(), &backup)?;
    replace(&backup.join(HELPER))?;
    if let Err(error) = sync() {
        restore(&backup).with_context(|| {
            format!("SMTP sync failed ({error:#}); restoring the old helper also failed")
        })?;
        return Err(error.context("SMTP sync failed; restored and ran the previous helper"));
    }
    Ok(())
}

fn restore(backup: &Path) -> Result<()> {
    let live = fs::symlink_metadata(Path::new(BIN_DIR).join(HELPER))?;
    let previous = backup.join("previous");
    let is_live = |file: &Path| -> Result<bool> {
        let meta = fs::metadata(file)?;
        Ok(live.dev() == meta.dev() && live.ino() == meta.ino())
    };
    ensure!(
        is_live(&backup.join(HELPER))? || is_live(&previous)?,
        "the installed helper differs from this deployment; inspect newer deployments first"
    );
    run(Command::new("sha256sum").current_dir(backup).args([
        "--check",
        "--strict",
        "files.sha256",
    ]))?;
    replace(&previous)?;
    sync()
}

pub fn rollback(id: &str) -> Result<()> {
    let _lock = lock()?;
    restore(&state(id))
}
