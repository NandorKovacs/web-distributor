use crate::run;
use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{fchown, symlink, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{tempdir_in, Builder, NamedTempFile};

const V2: &str = "/etc/web-distributor/v2";
const MASTER: &str = "/etc/caddy/Caddyfile";
const LIVE: &str = "/usr/bin/web-distributor";
const RELEASES: &str = "/opt/web-distributor";
const CLI: &str = "web-distributor";

fn mode(path: &Path, value: u32) -> Result<()> {
    fs::set_permissions(path, Permissions::from_mode(value))
        .with_context(|| format!("chmod {value:o} {}", path.display()))
}

fn config_lock() -> Result<File> {
    // The route CLI uses this same lock for every command that writes.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(Path::new(V2).join("config.toml.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn deployment_lock() -> Result<File> {
    let directory = File::open(V2).context("open deployment directory")?;
    fs2::FileExt::lock_exclusive(&directory).context("lock guest deployment")?;
    Ok(directory)
}

fn verify_cli(directory: &Path, manifest: &Path) -> Result<()> {
    let expected = fs::read_to_string(manifest).context("read CLI checksum manifest")?;
    let fields: Vec<_> = expected.split_ascii_whitespace().collect();
    ensure!(
        fields.len() == 2
            && fields[0].len() == 64
            && fields[0].bytes().all(|b| b.is_ascii_hexdigit())
            && fields[1] == CLI,
        "expected a single web-distributor SHA-256 checksum"
    );
    run(Command::new("sha256sum")
        .args(["--check", "--strict"])
        .arg(manifest)
        .current_dir(directory))
}

fn install_cli(release: &str, incoming: &Path) -> Result<PathBuf> {
    let incoming = incoming.canonicalize().context("resolve incoming bundle")?;
    let manifest = incoming.join("cli.sha256");
    verify_cli(&incoming, &manifest)?;
    let root = Path::new(RELEASES).join(release);
    let bin = root.join("bin");
    if !root.try_exists()? {
        let stage = tempdir_in(RELEASES)?;
        let staged_bin = stage.path().join("bin");
        fs::create_dir(&staged_bin)?;
        mode(&staged_bin, 0o755)?;
        run(Command::new("install")
            .args(["-o", "root", "-g", "root", "-m", "0755"])
            .arg(incoming.join(CLI))
            .arg(staged_bin.join(CLI)))?;
        verify_cli(&staged_bin, &manifest)?;
        mode(stage.path(), 0o755)?;
        fs::rename(stage.path(), &root).context("publish immutable CLI release")?;
    } else {
        verify_cli(&bin, &manifest)
            .context("immutable release already exists with a different or missing CLI")?;
    }
    let executable = bin.join(CLI);
    run(Command::new(&executable).arg("--version"))?;
    Ok(executable)
}

fn same_fragment(a: &Path, b: &Path) -> Result<bool> {
    let meta = |path: &Path| -> Result<_> {
        let m =
            fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
        ensure!(m.is_file(), "{} must be a regular file", path.display());
        Ok((m.uid(), m.gid(), m.mode(), m.len()))
    };
    Ok(meta(a)? == meta(b)? && fs::read(a)? == fs::read(b)?)
}

fn copy_fragment(from: &Path, to: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(from)?.is_file(),
        "expected a regular fragment"
    );
    run(Command::new("cp")
        .arg("--preserve=all")
        .arg("--")
        .arg(from)
        .arg(to))
}

fn replace_fragment(from: &Path, to: &Path) -> Result<()> {
    let stage = tempdir_in(to.parent().unwrap())?;
    let copy = stage.path().join("fragment");
    copy_fragment(from, &copy)?;
    fs::rename(copy, to).context("replace live Caddy fragment")
}

fn replace_link(target: &Path, live: &Path) -> Result<()> {
    let stage = tempdir_in(live.parent().unwrap())?;
    let link = stage.path().join(CLI);
    symlink(target, &link)?;
    fs::rename(link, live).context("replace live CLI symlink")
}

fn substitute_import(master: &str, live: &str, staged: &str) -> Result<String> {
    let mut count = 0;
    let mut result = String::with_capacity(master.len());
    for line in master.split_inclusive('\n') {
        if line.split_ascii_whitespace().eq(["import", live]) {
            count += 1;
            result.push_str(&line.replacen(live, staged, 1));
        } else {
            result.push_str(line);
        }
    }
    ensure!(
        count == 1,
        "expected exactly one standalone import of {live}, found {count}"
    );
    Ok(result)
}

fn validate(fragment: &Path, replacement: &Path) -> Result<()> {
    let master = Path::new(MASTER);
    let text = substitute_import(
        &fs::read_to_string(master)?,
        fragment.to_str().context("invalid live fragment path")?,
        replacement
            .to_str()
            .context("invalid staged fragment path")?,
    )?;
    let mut check = Builder::new()
        .prefix(".web-distributor-")
        .tempfile_in(master.parent().unwrap())?;
    check.write_all(text.as_bytes())?;
    fchown(check.as_file(), Some(0), Some(fs::metadata(V2)?.gid()))?;
    mode(check.path(), 0o640)?;
    run(Command::new("runuser")
        .args(["-u", "caddy", "--", "caddy", "validate", "--config"])
        .arg(check.path())
        .args(["--adapter", "caddyfile"]))
}

fn check_service() -> Result<()> {
    run(Command::new("systemctl").args(["is-active", "--quiet", "caddy"]))
}

fn reload() -> Result<()> {
    run(Command::new("systemctl").args(["reload", "caddy"]))
}

// Published only after validation. Snapshots never share inodes with live files.
struct Deployment {
    work: PathBuf,
    bin: PathBuf,
    live: PathBuf,
    fragment: PathBuf,
}

impl Deployment {
    fn new(release: &str) -> Self {
        Self {
            work: Path::new(V2).join(format!(".deploy-{release}")),
            bin: Path::new(RELEASES).join(release).join("bin").join(CLI),
            live: LIVE.into(),
            fragment: Path::new(V2).join("Caddyfile"),
        }
    }

    fn phase(&self) -> Result<String> {
        fs::read_to_string(self.work.join("phase"))
            .with_context(|| format!("read deployment state in {}", self.work.display()))
    }

    fn record(&self, phase: &str) -> Result<()> {
        let mut file = NamedTempFile::new_in(&self.work)?;
        file.write_all(phase.as_bytes())?;
        file.persist(self.work.join("phase"))?;
        Ok(())
    }

    fn candidate(&self) -> PathBuf {
        self.work.join("candidate")
    }

    fn previous_fragment(&self) -> PathBuf {
        self.work.join("previous-fragment")
    }

    fn previous_target(&self) -> Result<PathBuf> {
        Ok(fs::read_link(self.work.join("previous-target"))?)
    }

    // Each rename can be interrupted. Accept either recorded endpoint for each
    // file, but never overwrite a newer deployment or an operator's edit.
    fn check_live(&self) -> Result<()> {
        let target = fs::read_link(&self.live).context("read live CLI symlink")?;
        ensure!(
            target == self.bin || target == self.previous_target()?,
            "live CLI {} belongs to a different deployment",
            target.display()
        );
        ensure!(
            same_fragment(&self.fragment, &self.candidate())?
                || same_fragment(&self.fragment, &self.previous_fragment())?,
            "live Caddy fragment changed since deployment"
        );
        Ok(())
    }

    fn check_active(&self) -> Result<()> {
        ensure!(
            fs::read_link(&self.live)? == self.bin,
            "release is no longer active"
        );
        ensure!(
            same_fragment(&self.fragment, &self.candidate())?,
            "active fragment changed since deployment"
        );
        run(Command::new(&self.live).arg("--version"))?;
        run(Command::new(&self.live).arg("list"))?;
        check_service()
    }

    fn restore(&self) -> Result<()> {
        let phase = self.phase()?;
        let target = self.previous_target()?;
        let previous = self.previous_fragment();
        if phase == "rolled-back" {
            ensure!(
                fs::read_link(&self.live)? == target && same_fragment(&self.fragment, &previous)?,
                "release was already rolled back and live state has since changed"
            );
            return check_service();
        }
        ensure!(
            matches!(phase.as_str(), "prepared" | "active" | "restoring"),
            "unknown deployment state {phase:?}"
        );
        if phase == "active" {
            ensure!(
                fs::read_link(&self.live)? == self.bin
                    && same_fragment(&self.fragment, &self.candidate())?,
                "active CLI or fragment changed since deployment"
            );
        }
        self.check_live()?;
        let target_file = fs::metadata(self.live.parent().unwrap().join(&target))
            .with_context(|| format!("find previous CLI {}", target.display()))?;
        ensure!(
            target_file.is_file() && target_file.mode() & 0o111 != 0,
            "previous CLI is not executable"
        );
        validate(&self.fragment, &previous).context("validate previous fragment")?;
        self.record("restoring")?;
        if fs::read_link(&self.live)? != target {
            replace_link(&target, &self.live)?;
        }
        if !same_fragment(&self.fragment, &previous)? {
            replace_fragment(&previous, &self.fragment)?;
        }
        // Retry even after both renames: the prior invocation may have stopped
        // before systemd finished loading the restored on-disk configuration.
        if !same_fragment(&previous, &self.candidate())? {
            reload()?;
        }
        check_service()?;
        self.record("rolled-back")
    }

    fn promote(&self) -> Result<()> {
        self.check_live()?;
        let attempt = || -> Result<()> {
            replace_link(&self.bin, &self.live)?;
            if !same_fragment(&self.candidate(), &self.previous_fragment())? {
                replace_fragment(&self.candidate(), &self.fragment)?;
                reload()?;
            }
            self.check_active()?;
            self.record("active")
        };
        if let Err(error) = attempt() {
            match self.restore() {
                Ok(()) => bail!("activation failed and was rolled back: {error:#}"),
                Err(recovery) => {
                    bail!("activation failed: {error:#}. Rollback failed: {recovery:#}")
                }
            }
        }
        Ok(())
    }
}

pub fn activate(release: &str, incoming: &Path) -> Result<()> {
    let _deployment = deployment_lock()?;
    check_service()?;
    let bin = install_cli(release, incoming)?;
    let d = Deployment::new(release);
    let retry = d.work.try_exists()?;
    if retry {
        match d.phase()?.as_str() {
            "active" => {
                let _config = config_lock()?;
                return d.check_active();
            }
            "prepared" => {}
            "restoring" => bail!("rollback was interrupted, retry rollback for {release}"),
            "rolled-back" => {
                bail!("release was rolled back, use a new release suffix to activate again")
            }
            phase => bail!("unknown deployment state {phase:?}"),
        }
    }
    let config = Path::new(V2).join("config.toml");
    ensure!(
        fs::metadata(&config)?.len() > 0,
        "expected nonempty {}",
        config.display()
    );
    run(Command::new("install").args(["-d", "-o", "root", "-g", "caddy", "-m", "2750", V2]))?;
    let stage = Builder::new().prefix(".preparing-").tempdir_in(V2)?;
    mode(stage.path(), 0o2750)?;
    let candidate = stage.path().join("candidate");
    let old = stage.path().join("previous-fragment");
    let previous = fs::read_link(&d.live).context("expected a live CLI symlink")?;
    let current = fs::metadata(&d.live)?;
    ensure!(
        current.is_file() && current.mode() & 0o111 != 0,
        "live CLI is not executable"
    );
    if !retry {
        ensure!(
            previous != bin,
            "release is already selected without rollback records"
        );
        symlink(&previous, stage.path().join("previous-target"))?;
        copy_fragment(&d.fragment, &old)?;
    }
    // The generator takes the CLI's config lock itself. Acquire it afterwards
    // and check the captured live state before publishing either file.
    run(Command::new(&bin)
        .arg("-c")
        .arg(&config)
        .arg("--output")
        .arg(&candidate)
        .arg("generate"))?;
    let _config = config_lock()?;
    let m = fs::symlink_metadata(&candidate)?;
    ensure!(
        m.is_file()
            && m.uid() == 0
            && m.gid() == fs::metadata(V2)?.gid()
            && m.mode() & 0o7777 == 0o640,
        "candidate must be root:caddy mode 0640"
    );
    if retry {
        ensure!(
            same_fragment(&candidate, &d.candidate())?,
            "source data changed since preparation"
        );
        d.check_live()?;
    } else {
        ensure!(
            previous == fs::read_link(&d.live)? && same_fragment(&old, &d.fragment)?,
            "live state changed during generation"
        );
    }
    validate(&d.fragment, &candidate).context("validate candidate fragment")?;
    if !retry {
        fs::write(stage.path().join("phase"), "prepared")?;
        fs::rename(stage.path(), &d.work).context("publish rollback records")?;
    }
    d.promote()?;
    if let Err(error) =
        run(Command::new("journalctl").args(["-u", "caddy", "-n", "25", "--no-pager"]))
    {
        eprintln!("Could not display Caddy journal: {error:#}");
    }
    Ok(())
}

pub fn rollback(release: &str) -> Result<()> {
    let _deployment = deployment_lock()?;
    let _config = config_lock()?;
    Deployment::new(release).restore()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_one_standalone_import_preserving_rest_of_master() {
        let master = "# import /old\n\timport\t/old\r\nimport ./sites/*";
        assert_eq!(
            substitute_import(master, "/old", "/new").unwrap(),
            "# import /old\n\timport\t/new\r\nimport ./sites/*"
        );
        for invalid in [
            "# import /old",
            "import /old arg",
            "import /old\nimport /old",
        ] {
            assert!(substitute_import(invalid, "/old", "/new").is_err());
        }
    }

    #[test]
    fn rollback_guard_handles_interrupted_promotion_and_rejects_drift() {
        let dir = tempfile::tempdir().unwrap();
        let d = Deployment {
            work: dir.path().into(),
            bin: dir.path().join("release"),
            live: dir.path().join("cli"),
            fragment: dir.path().join("fragment"),
        };
        let old = Path::new("relative-old-target");
        symlink(old, d.work.join("previous-target")).unwrap();
        symlink(old, &d.live).unwrap();
        fs::write(d.previous_fragment(), "previous").unwrap();
        fs::write(d.candidate(), "next").unwrap();
        fs::write(&d.fragment, "previous").unwrap();
        d.check_live().unwrap();
        replace_link(&d.bin, &d.live).unwrap();
        d.check_live().unwrap();
        replace_fragment(&d.candidate(), &d.fragment).unwrap();
        d.check_live().unwrap();
        fs::write(&d.fragment, "new route").unwrap();
        assert!(d.check_live().is_err());
        assert_eq!(fs::read_to_string(d.candidate()).unwrap(), "next");
        replace_fragment(&d.previous_fragment(), &d.fragment).unwrap();
        replace_link(Path::new("later-release"), &d.live).unwrap();
        assert!(d.check_live().is_err());
    }
}
