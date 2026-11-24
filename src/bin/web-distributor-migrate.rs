use clap::{Parser, Subcommand};
use fs2::FileExt;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const LETS_ENCRYPT_ISSUER: &str = "acme-v02.api.letsencrypt.org-directory";

#[derive(Parser)]
#[command(about = "Manage Caddy-issued TLS certificates for SMTP")]
struct Cli {
    #[command(subcommand)]
    command: SmtpTlsCommand,
}

#[derive(Subcommand)]
enum SmtpTlsCommand {
    /// Pull and verify a Caddy-managed TLS pair for SMTP
    PullSmtpTls {
        /// Incus instance, optionally prefixed with a remote
        #[arg(long)]
        instance: String,
        /// Exact DNS name of the SMTP certificate
        #[arg(long)]
        hostname: String,
        /// Absolute Caddy filesystem storage root inside the instance
        #[arg(long)]
        storage: PathBuf,
        /// Empty private directory to receive fullchain.pem and privkey.pem
        #[arg(long)]
        output: PathBuf,
    },
    /// Sync a verified Caddy-managed pair to Postfix in an Incus instance
    SyncSmtpTls {
        /// Caddy Incus instance in the default project
        #[arg(long)]
        instance: String,
        /// Exact DNS name of the SMTP certificate
        #[arg(long)]
        hostname: String,
        /// Absolute Caddy filesystem storage root inside the instance
        #[arg(long)]
        storage: PathBuf,
        /// Postfix Incus instance
        #[arg(long)]
        destination: String,
        /// Incus project of the Postfix instance
        #[arg(long)]
        destination_project: String,
        /// Existing stable host file shared by all invocations for this destination
        #[arg(long)]
        lock_file: PathBuf,
    },
}

fn write_private_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    if let Err(error) = file.write_all(contents) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn incus(project: Option<&str>) -> Command {
    let mut command = Command::new("incus");
    if let Some(project) = project {
        command.args(["--project", project]);
    }
    command
}

fn pull(project: Option<&str>, instance: &str, source: &Path) -> Result<Vec<u8>> {
    let mut remote = std::ffi::OsString::from(instance);
    remote.push(source);
    let output = incus(project)
        .args(["file", "pull", "--quiet", "--"])
        .arg(&remote)
        .arg("-")
        .output()?;
    if !output.status.success() {
        return Err(format!("could not pull {}", source.display()).into());
    }
    Ok(output.stdout)
}

fn openssl_public_key(arguments: &[&str], data: &[u8]) -> Result<Vec<u8>> {
    let mut command = Command::new("openssl")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let input_error = command.stdin.take().unwrap().write_all(data).err();
    let output = command.wait_with_output()?;
    if !output.status.success() || input_error.is_some() || output.stdout.is_empty() {
        return Err("could not read public key from pulled TLS material".into());
    }
    Ok(output.stdout)
}

fn matched_pair(instance: &str, certificate: &Path, key: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut error = "could not read the certificate pair".into();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        let pair = (|| {
            let certificate = pull(None, instance, certificate)?;
            let key = pull(None, instance, key)?;
            if openssl_public_key(&["x509", "-noout", "-pubkey"], &certificate)?
                != openssl_public_key(&["pkey", "-pubout", "-passin", "pass:"], &key)?
            {
                return Err("certificate and private key do not match".into());
            }
            Ok((certificate, key))
        })();
        match pair {
            Ok(pair) => return Ok(pair),
            Err(failure) => error = failure,
        }
    }
    Err(error)
}

fn pull_smtp_tls(instance: &str, hostname: &str, storage: &Path, output: &Path) -> Result<()> {
    if hostname.is_empty()
        || !hostname.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        || hostname.starts_with('.')
        || hostname.ends_with('.')
    {
        return Err("hostname must be a lowercase DNS name".into());
    }
    if instance.is_empty() || instance.contains('/') || !storage.is_absolute() {
        return Err("provide an Incus instance and an absolute storage path".into());
    }
    if output.metadata()?.permissions().mode() & 0o077 != 0 || output.read_dir()?.next().is_some() {
        return Err("output must be an empty private directory".into());
    }

    let site = storage
        .join("certificates")
        .join(LETS_ENCRYPT_ISSUER)
        .join(hostname);
    let (certificate, key) = matched_pair(
        instance,
        &site.join(format!("{hostname}.crt")),
        &site.join(format!("{hostname}.key")),
    )?;

    let fullchain = output.join("fullchain.pem");
    let privkey = output.join("privkey.pem");
    write_private_new(&fullchain, &certificate)?;
    let result = (|| {
        let verify = Command::new("openssl")
            .args([
                "verify",
                "-verify_hostname",
                hostname,
                "-purpose",
                "sslserver",
                "-untrusted",
            ])
            .arg(&fullchain)
            .arg(&fullchain)
            .output()?;
        if !verify.status.success() {
            let reason = String::from_utf8_lossy(&verify.stderr);
            return Err(format!(
                "certificate trust, validity or hostname verification failed: {}",
                reason.trim()
            )
            .into());
        }
        write_private_new(&privkey, &key)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(fullchain);
    }
    result
}

const POSTFIX_TLS_DIR: &str = "/etc/postfix/tls";
const POSTFIX_RELOAD_PENDING: &str = "/etc/postfix/tls/.reload-pending";
const POSTFIX_STAGE_PREFIX: &str = "/etc/postfix/.tls-sync.";

struct PostfixTarget<'a> {
    instance: &'a str,
    project: &'a str,
}

impl PostfixTarget<'_> {
    fn exec(&self, args: &[&str]) -> io::Result<Output> {
        incus(Some(self.project))
            .args(["exec", "--disable-stdin", self.instance, "--"])
            .args(args)
            .output()
    }

    fn check(&self, args: &[&str]) -> Result<Output> {
        let output = self.exec(args)?;
        if !output.status.success() {
            return Err(command_error(args[0], &output));
        }
        Ok(output)
    }

    fn push(&self, source: &Path, destination: &str, mode: &str) -> Result<()> {
        let output = incus(Some(self.project))
            .args(["file", "push", "--quiet", "--uid", "0", "--gid", "0"])
            .args(["--mode", mode, "--"])
            .arg(source)
            .arg(format!("{}{destination}", self.instance))
            .output()?;
        if !output.status.success() {
            return Err(command_error("file push", &output));
        }
        Ok(())
    }
}

fn command_error(operation: &str, output: &Output) -> Box<dyn Error> {
    format!(
        "Postfix destination {operation} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into()
}

fn install_postfix_tls(
    target: &PostfixTarget<'_>,
    source: &Path,
    changed: &[(&str, &str)],
) -> Result<()> {
    target.check(&["mkdir", "-p", "-m", "0755", POSTFIX_TLS_DIR])?;
    target.check(&["test", "!", "-L", POSTFIX_TLS_DIR])?;
    let template = format!("{POSTFIX_STAGE_PREFIX}XXXXXXXX");
    let stage = target.check(&["mktemp", "-d", &template])?;
    let stage = std::str::from_utf8(&stage.stdout)?.trim();
    if !stage
        .strip_prefix(POSTFIX_STAGE_PREFIX)
        .is_some_and(|nonce| {
            nonce.len() == 8 && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    {
        return Err("unexpected Postfix TLS staging directory".into());
    }

    let install = (|| {
        let staged = format!("{stage}/tls");
        target.check(&["cp", "-a", "--", POSTFIX_TLS_DIR, &staged])?;
        for &(name, mode) in changed {
            let path = format!("{staged}/{name}");
            target.check(&["rm", "-f", "--", &path])?;
            target.push(&source.join(name), &path, mode)?;
        }
        // Promote both files and the reload marker in one filesystem operation.
        target.check(&["touch", &format!("{staged}/.reload-pending")])?;
        target.check(&[
            "mv",
            "--exchange",
            "--no-copy",
            "-T",
            "--",
            &staged,
            POSTFIX_TLS_DIR,
        ])?;
        Ok(())
    })();
    let cleanup = target.check(&["rm", "-rf", "--", stage]);
    install.and(cleanup)?;
    Ok(())
}

fn sync_smtp_tls(
    instance: &str,
    hostname: &str,
    storage: &Path,
    target: PostfixTarget<'_>,
    lock_file: &Path,
) -> Result<()> {
    if target.instance.is_empty()
        || target.instance.starts_with('-')
        || target.instance.contains('/')
        || target.project.is_empty()
    {
        return Err("provide a Postfix Incus instance and project".into());
    }

    let lock = File::open(lock_file)?;
    FileExt::lock_exclusive(&lock)?;
    let source = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    pull_smtp_tls(instance, hostname, storage, source.path())?;

    let mut changed = Vec::new();
    for (name, mode) in [("fullchain.pem", "0644"), ("privkey.pem", "0600")] {
        let path = format!("{POSTFIX_TLS_DIR}/{name}");
        let expected = fs::read(source.path().join(name))?;
        let actual = pull(Some(target.project), target.instance, Path::new(&path));
        let metadata = target.exec(&["stat", "-c", "%u:%g:%a", "--", &path])?;
        let wanted_metadata = format!("0:0:{}\n", &mode[1..]);
        if !actual.is_ok_and(|content| content == expected)
            || !metadata.status.success()
            || metadata.stdout != wanted_metadata.as_bytes()
        {
            changed.push((name, mode));
        }
    }
    if changed.is_empty() {
        let marker = target.exec(&["test", "-f", POSTFIX_RELOAD_PENDING])?;
        match marker.status.code() {
            Some(0) => {}
            Some(1) if marker.stderr.is_empty() => {
                println!("SMTP TLS files are already current.");
                return Ok(());
            }
            _ => return Err(command_error("reload marker check", &marker)),
        }
    } else {
        println!("Installing updated SMTP TLS files in {}.", target.instance);
        install_postfix_tls(&target, source.path(), &changed)?;
    }

    let postfix = target.exec(&["systemctl", "is-active", "--quiet", "postfix"])?;
    match postfix.status.code() {
        Some(0) => {
            println!("Reloading Postfix.");
            target.check(&["systemctl", "reload", "postfix"])?;
            target.check(&["rm", "-f", POSTFIX_RELOAD_PENDING])?;
        }
        Some(3 | 4) if postfix.stderr.is_empty() => {}
        _ => return Err(command_error("Postfix status check", &postfix)),
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let result = match Cli::parse().command {
        SmtpTlsCommand::PullSmtpTls {
            instance,
            hostname,
            storage,
            output,
        } => pull_smtp_tls(&instance, &hostname, &storage, &output),
        SmtpTlsCommand::SyncSmtpTls {
            instance,
            hostname,
            storage,
            destination,
            destination_project,
            lock_file,
        } => sync_smtp_tls(
            &instance,
            &hostname,
            &storage,
            PostfixTarget {
                instance: &destination,
                project: &destination_project,
            },
            &lock_file,
        ),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
