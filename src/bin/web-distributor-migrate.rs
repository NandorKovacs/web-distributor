use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use toml::{Table, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const LETS_ENCRYPT_ISSUER: &str = "acme-v02.api.letsencrypt.org-directory";

#[derive(Parser)]
#[command(about = "Prepare a Caddy migration from a private Nginx/ACME snapshot")]
struct Cli {
    #[command(subcommand)]
    command: MigrationCommand,
}

#[derive(Subcommand)]
enum MigrationCommand {
    /// Compare legacy outputs and write a candidate configuration; performs no service changes
    Prepare {
        /// Directory with etc/web-distributor.toml and nginx-effective.conf
        #[arg(long)]
        snapshot: PathBuf,
        /// New private TOML file; existing files are never replaced
        #[arg(long)]
        output: PathBuf,
    },
    /// Pull and verify a Caddy-managed TLS pair for the existing SMTP sync job
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
}

fn read_toml(path: &Path) -> Result<Table> {
    let source =
        fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    source
        .parse::<Table>()
        .map_err(|error| format!("{}: {error}", path.display()).into())
}

fn config_string<'a>(table: &'a Table, field: &str) -> Result<&'a str> {
    table
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {field}").into())
}

fn snapshot_path(root: &Path, live_path: &str) -> Result<PathBuf> {
    let path = Path::new(live_path);
    let relative = path.strip_prefix("/")?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("unsupported snapshot path {live_path}").into());
    }
    Ok(root.join(relative))
}

fn active_lines(nginx: &str) -> Vec<&str> {
    nginx
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn expected_nginx(config: &Table, home: &str, from: &str, to: &str) -> String {
    let get = |field: &str| config.get(field)?.as_table()?.get(from)?.as_str();
    let login_group = get("login_groups").map_or_else(String::new, |name| {
        format!("auth_basic \"login\";\n        auth_basic_user_file {home}/login_groups/{name};")
    });
    let (auth_request, auth_outpost) = get("forward_auth").map_or_else(
        || ("", String::new()),
        |outpost| {
            (
                include_str!("../../ops/legacy-nginx/nginx-authrequest.conf"),
                format!(
                    include_str!("../../ops/legacy-nginx/nginx-outpost.conf"),
                    outpost = outpost
                ),
            )
        },
    );
    let grpc_location = get("grpc_prefix").map_or_else(String::new, |prefix| {
        format!(
            include_str!("../../ops/legacy-nginx/nginx-grpc.conf"),
            prefix = prefix,
            to = to
        )
    });
    format!(
        include_str!("../../ops/legacy-nginx/nginx.conf"),
        from = from,
        to = to,
        login_group = login_group,
        auth_request = auth_request,
        auth_outpost = auth_outpost,
        grpc_location = grpc_location,
    )
}

fn review_nginx(snapshot: &Path, config: &Table, routes: &Table) -> Result<()> {
    let home = config_string(config, "home")?;
    let directory = snapshot_path(snapshot, home)?.join("nginx");
    let effective = fs::read_to_string(snapshot.join("nginx-effective.conf"))?;
    let mut generated = BTreeSet::new();
    for (domain, target) in routes {
        if domain.is_empty()
            || !domain
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(format!("invalid route name {domain}").into());
        }

        let filename = format!("{domain}.nginx");
        let target = target
            .as_str()
            .ok_or_else(|| format!("invalid target for {domain}"))?;
        let existing = fs::read_to_string(directory.join(&filename))?;
        let expected = expected_nginx(config, home, domain, target);
        if active_lines(&existing) != active_lines(&expected) {
            println!("Review locally modified Nginx vhost {filename}");
        }
        generated.insert(format!("{home}/nginx/{filename}"));
    }
    for entry in directory.read_dir()? {
        let entry = entry?;
        if !generated.contains(&format!("{home}/nginx/{}", entry.file_name().display())) {
            println!("Review extra Nginx include {}", entry.path().display());
        }
    }

    let mut loaded = BTreeSet::new();
    for source in effective.split("# configuration file ").skip(1) {
        let Some((header, content)) = source.split_once('\n') else {
            continue;
        };
        let Some(path) = header.strip_suffix(':') else {
            continue;
        };
        if generated.contains(path) {
            loaded.insert(path);
            let captured = fs::read_to_string(snapshot_path(snapshot, path)?)?;
            if active_lines(content) != active_lines(&captured) {
                println!("Review active Nginx vhost differing from snapshot file {path}");
            }
        } else if !path.ends_with("/mime.types") {
            println!("Review non-generated active Nginx configuration {path}");
        }
    }
    for path in generated {
        if !loaded.contains(path.as_str()) {
            println!("Review generated vhost absent from effective Nginx configuration {path}");
        }
    }
    Ok(())
}

fn certificate_only(snapshot: &Path, config: &Table, routes: &Table) -> Result<BTreeSet<String>> {
    let acme_dir = snapshot_path(snapshot, config_string(config, "acme_redirect_configs")?)?;
    let mut names = BTreeSet::new();
    let mut route_certificates = BTreeSet::new();
    for entry in acme_dir.read_dir()? {
        let path = entry?.path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("conf") {
            continue;
        }

        let file = read_toml(&path)?;
        let cert = file
            .get("cert")
            .and_then(Value::as_table)
            .ok_or_else(|| format!("missing [cert] in {}", path.display()))?;
        let subject = config_string(cert, "name")?;
        let dns_names = cert
            .get("dns_names")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("missing dns_names in {}", path.display()))?;

        if cert
            .keys()
            .any(|key| !["name", "dns_names", "exec"].contains(&key.as_str()))
            || cert.get("exec").is_some_and(|exec| {
                exec != &Value::Array(vec![Value::String("systemctl reload nginx".into())])
            })
        {
            println!("Review custom ACME settings or hooks in {}", path.display());
        }

        let [Value::String(hostname)] = dns_names.as_slice() else {
            println!(
                "Review multi-name or invalid legacy certificate {}",
                path.display()
            );
            continue;
        };
        if subject != hostname || hostname.starts_with("*.") {
            println!(
                "Review custom legacy certificate {} for {hostname}",
                path.display()
            );
            continue;
        }

        if routes.contains_key(hostname) {
            if !route_certificates.insert(hostname.clone()) {
                println!("Review duplicate legacy web certificate for {hostname}");
            }
        } else if names.insert(hostname.clone()) {
            println!("Carry forward certificate-only hostname {hostname}");
        } else {
            println!("Review duplicate legacy certificate-only hostname {hostname}");
        }
    }

    for hostname in routes.keys() {
        if !route_certificates.contains(hostname) {
            println!("Review web route with no matching single-name legacy certificate {hostname}");
        }
    }

    Ok(names)
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

fn prepare(snapshot: &Path, output: &Path) -> Result<()> {
    let mut config = read_toml(&snapshot.join("etc/web-distributor.toml"))?;
    let routes = config
        .get("routes")
        .and_then(Value::as_table)
        .ok_or("missing [routes]")?;

    review_nginx(snapshot, &config, routes)?;
    let certificate_only = certificate_only(snapshot, &config, routes)?;
    for field in ["login_groups", "forward_auth", "grpc_prefix"] {
        if let Some(entries) = config.get(field).and_then(Value::as_table) {
            for hostname in entries
                .keys()
                .filter(|hostname| !routes.contains_key(*hostname))
            {
                println!("Retain inactive [{field}] assignment for {hostname}");
            }
        }
    }
    println!("Reviewed {} configured web routes", routes.len());

    config.remove("acme_redirect_configs");
    let existing = config
        .entry("certificate_only")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("invalid certificate_only")?;
    for name in certificate_only.into_iter().map(Value::String) {
        if !existing.contains(&name) {
            existing.push(name);
        }
    }
    write_private_new(output, toml::to_string_pretty(&config)?.as_bytes())?;
    println!("Wrote candidate configuration {}", output.display());
    Ok(())
}

fn pull(instance: &str, source: &Path) -> Result<Vec<u8>> {
    let mut remote = std::ffi::OsString::from(instance);
    remote.push(source);
    let output = Command::new("incus")
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
            let certificate = pull(instance, certificate)?;
            let key = pull(instance, key)?;
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

fn main() -> std::process::ExitCode {
    let result = match Cli::parse().command {
        MigrationCommand::Prepare { snapshot, output } => prepare(&snapshot, &output),
        MigrationCommand::PullSmtpTls {
            instance,
            hostname,
            storage,
            output,
        } => pull_smtp_tls(&instance, &hostname, &storage, &output),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
