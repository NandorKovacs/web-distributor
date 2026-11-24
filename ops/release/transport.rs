use crate::{output, run};
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::path::Path;
use std::process::Command;

const INCUS: &str = "incus --force-local --project default";
const INSTANCE: &str = "web-distributor";

#[derive(Clone, Copy, Debug)]
pub enum Destination {
    Guest,
    Smtp,
}

fn ssh(tty: bool, command: &str) -> Command {
    let mut ssh = Command::new("ssh");
    ssh.args([if tty { "-t" } else { "-T" }, "cobalt", command]);
    ssh
}

impl Destination {
    fn remote(self, command: &str) -> Command {
        match self {
            Self::Guest => ssh(false, &format!("{INCUS} exec {INSTANCE} -T -- {command}")),
            Self::Smtp => ssh(false, command),
        }
    }

    fn send(self, file: &Path, remote_path: &str) -> Result<()> {
        match self {
            Self::Guest => run(ssh(
                false,
                &format!("{INCUS} file push --mode 0700 - {INSTANCE}{remote_path}"),
            )
            .stdin(File::open(file).with_context(|| format!("open {}", file.display()))?)),
            Self::Smtp => run(Command::new("scp")
                .arg("--")
                .arg(file)
                .arg(format!("cobalt:{remote_path}"))),
        }
    }

    pub fn execute(self, id: &str, bundle: Option<&Path>) -> Result<()> {
        let (prefix, apply, restore, binary, checksum) = match self {
            Self::Guest => (
                "/root/web-distributor.",
                "apply-guest",
                "restore-guest",
                "web-distributor",
                "cli.sha256",
            ),
            Self::Smtp => (
                "/tmp/web-distributor.",
                "apply-smtp",
                "restore-smtp",
                "web-distributor-migrate",
                "helper.sha256",
            ),
        };
        let driver = std::env::current_exe()?;
        let incoming = output(&mut self.remote(&format!("mktemp -d {prefix}XXXXXXXXXX")))?;
        // OpenSSH passes remote commands through a shell. Only fixed tokens,
        // validated release IDs and this checked directory may enter a command.
        ensure!(
            incoming
                .strip_prefix(prefix)
                .is_some_and(|s| s.len() == 10 && s.bytes().all(|b| b.is_ascii_alphanumeric())),
            "unexpected remote staging path {incoming:?}"
        );
        let result = (|| {
            let tool = format!("{incoming}/release-tool");
            self.send(&driver, &tool)?;
            let command = if let Some(bundle) = bundle {
                self.send(&bundle.join(binary), &format!("{incoming}/{binary}"))?;
                self.send(&bundle.join(checksum), &format!("{incoming}/{checksum}"))?;
                format!("{tool} {apply} {id} {incoming}")
            } else {
                format!("{tool} {restore} {id}")
            };
            match self {
                Self::Guest => run(&mut self.remote(&command)),
                Self::Smtp => run(&mut ssh(true, &format!("sudo -- {command}"))),
            }
        })();
        if let Err(error) = run(&mut self.remote(&format!("rm -rf -- {incoming}"))) {
            eprintln!("Could not remove temporary directory {incoming}: {error:#}");
        }
        result
    }
}
