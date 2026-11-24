mod caddy;
mod config;
mod credentials;
mod files;

use anyhow::{bail, ensure, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use credentials::Accounts;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Manage routes and generate a Caddy configuration fragment"
)]
struct Cli {
    /// Source configuration file
    #[arg(
        short,
        long,
        global = true,
        default_value = "/etc/web-distributor/v2/config.toml"
    )]
    config: PathBuf,

    /// Generated Caddyfile fragment, defaulting to <home>/Caddyfile
    #[arg(long, global = true, value_name = "PATH")]
    output: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate the Caddyfile from the current configuration
    Generate,
    /// Add or replace a reverse-proxy route
    Add {
        domain: String,
        target: String,

        #[arg(short, long)]
        force: bool,

        /// Serve native gRPC under this literal prefix, such as /inventory.v1.
        #[arg(short, long)]
        grpc_prefix: Option<String>,
    },
    /// Remove a reverse-proxy route
    Remove { domain: String },
    /// List the configured routes
    List,
    /// Manage HTTP Basic Auth accounts and route assignments
    LoginGroup {
        #[command(subcommand)]
        command: PasswordCommands,
    },
    /// Manage Authentik forward-auth route assignments
    ForwardAuth {
        #[command(subcommand)]
        command: ForwardAuthCommands,
    },
}

#[derive(Subcommand)]
enum PasswordCommands {
    Create {
        login_group: String,
    },
    Remove {
        login_group: String,
    },
    List,
    Apply {
        domain: String,
        login_group: String,

        #[arg(short, long)]
        force: bool,
    },
    Disable {
        domain: String,
    },
    AddLogin {
        login_group: String,
        name: String,
        password: Option<String>,

        #[arg(short, long)]
        force: bool,
    },
    RevokeLogin {
        login_group: String,
        name: String,
    },
}

#[derive(Subcommand)]
enum ForwardAuthCommands {
    Apply {
        domain: String,
        outpost: String,

        #[arg(short, long)]
        force: bool,
    },
    Disable {
        domain: String,
    },
    List,
}

enum Change {
    Generate,
    WriteConfig,
    WriteLogins {
        group: String,
        path: PathBuf,
        accounts: Accounts,
    },
    RemoveGroup(PathBuf),
}

fn login_command(
    command: PasswordCommands,
    config: &mut Config,
    login_dir: &Path,
) -> Result<Option<Change>> {
    let change = match command {
        PasswordCommands::Create { login_group } => {
            let path = credentials::group_path(login_dir, &login_group)?;
            files::create_login_group(&path)?;
            return Ok(None);
        }
        PasswordCommands::Remove { login_group } => {
            let path = credentials::group_path(login_dir, &login_group)?;
            credentials::check_file(&path)?;
            config.login_groups.retain(|_, group| group != &login_group);
            Change::RemoveGroup(path)
        }
        PasswordCommands::List => {
            for group in credentials::list(login_dir)? {
                println!("{group}");
            }
            return Ok(None);
        }
        PasswordCommands::Apply {
            domain,
            login_group,
            force,
        } => {
            ensure!(config.routes.contains_key(&domain), "no route for {domain}");
            ensure!(
                force || !config.login_groups.contains_key(&domain),
                "a login group is already assigned to {domain}. Use --force to replace it"
            );
            let path = credentials::group_path(login_dir, &login_group)?;
            credentials::check_file(&path)?;
            config.login_groups.insert(domain, login_group);
            Change::WriteConfig
        }
        PasswordCommands::Disable { domain } => {
            ensure!(
                config.login_groups.remove(&domain).is_some(),
                "no login group assigned to {domain}"
            );
            Change::WriteConfig
        }
        PasswordCommands::AddLogin {
            login_group,
            name,
            password,
            force,
        } => {
            credentials::validate_username(&name)?;
            let path = credentials::group_path(login_dir, &login_group)?;
            let mut accounts = credentials::read(&path)?;
            ensure!(
                force || !accounts.contains_key(&name),
                "login {name:?} already exists in {login_group}. Use --force to replace it"
            );
            let password = match password {
                Some(password) => password,
                None => rpassword::prompt_password("Enter password: ")?,
            };
            let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST).context("hash password")?;
            accounts.insert(name, hash);
            Change::WriteLogins {
                group: login_group,
                path,
                accounts,
            }
        }
        PasswordCommands::RevokeLogin { login_group, name } => {
            let path = credentials::group_path(login_dir, &login_group)?;
            let mut accounts = credentials::read(&path)?;
            ensure!(
                accounts.remove(&name).is_some(),
                "no login {name:?} in {login_group}"
            );
            Change::WriteLogins {
                group: login_group,
                path,
                accounts,
            }
        }
    };
    Ok(Some(change))
}

fn run(cli: Cli) -> Result<()> {
    let read_only = matches!(
        &cli.command,
        Commands::List
            | Commands::LoginGroup {
                command: PasswordCommands::List
            }
            | Commands::ForwardAuth {
                command: ForwardAuthCommands::List
            }
    );
    let _lock = if read_only {
        None
    } else {
        Some(files::lock_config(&cli.config)?)
    };
    let (mut config, has_config) = Config::read(&cli.config)?;
    let login_dir = config.home.join("login_groups");
    let output_path = cli.output.unwrap_or_else(|| config.home.join("Caddyfile"));
    if !has_config && !read_only {
        match fs::symlink_metadata(&output_path) {
            Ok(_) => bail!(
                "configuration {} is missing and output {} already exists",
                cli.config.display(),
                output_path.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("check {}", output_path.display()))
            }
        }
    }

    let change = match cli.command {
        Commands::Generate if has_config => Change::Generate,
        Commands::Generate => Change::WriteConfig,
        Commands::Add {
            domain,
            target,
            force,
            grpc_prefix,
        } => {
            if let Some(existing) = config.routes.get(&domain) {
                ensure!(force, "there is already a route from {domain} to {existing}. Use --force to replace it");
            }
            config.routes.insert(domain.clone(), target);
            if let Some(prefix) = grpc_prefix {
                config.grpc_prefix.insert(domain, prefix);
            } else {
                config.grpc_prefix.remove(&domain);
            }
            Change::WriteConfig
        }
        Commands::Remove { domain } => {
            ensure!(
                config.routes.remove(&domain).is_some(),
                "no route for {domain}"
            );
            config.grpc_prefix.remove(&domain);
            Change::WriteConfig
        }
        Commands::List => {
            for (domain, target) in &config.routes {
                print!("{domain} => {target}");
                if let Some(group) = config.login_groups.get(domain) {
                    print!(", login group: {group}");
                }
                println!();
            }
            return Ok(());
        }
        Commands::LoginGroup { command } => {
            match login_command(command, &mut config, &login_dir)? {
                Some(change) => change,
                None => return Ok(()),
            }
        }
        Commands::ForwardAuth { command } => match command {
            ForwardAuthCommands::Apply {
                domain,
                outpost,
                force,
            } => {
                ensure!(config.routes.contains_key(&domain), "no route for {domain}");
                ensure!(
                    force || !config.forward_auth.contains_key(&domain),
                    "forward auth is already assigned to {domain}. Use --force to replace it"
                );
                config.forward_auth.insert(domain, outpost);
                Change::WriteConfig
            }
            ForwardAuthCommands::Disable { domain } => {
                ensure!(
                    config.forward_auth.remove(&domain).is_some(),
                    "no forward auth assigned to {domain}"
                );
                Change::WriteConfig
            }
            ForwardAuthCommands::List => {
                for (domain, outpost) in &config.forward_auth {
                    println!("{domain} => {outpost}");
                }
                return Ok(());
            }
        },
    };

    let updated_group = match &change {
        Change::WriteLogins {
            group, accounts, ..
        } => Some((group.as_str(), accounts)),
        _ => None,
    };
    let rendered = caddy::render(&config, &login_dir, updated_group)?;
    let contents;
    let source = match &change {
        Change::Generate => None,
        Change::WriteConfig | Change::RemoveGroup(_) => {
            contents = toml::to_string(&config).context("serialize configuration")?;
            Some((cli.config.as_path(), contents.as_str(), 0o600))
        }
        Change::WriteLogins { path, accounts, .. } => {
            contents = credentials::serialize(accounts);
            Some((path.as_path(), contents.as_str(), 0o600))
        }
    };
    files::publish(&cli.config, &login_dir, &output_path, &rendered, source)?;
    if let Change::RemoveGroup(path) = change {
        fs::remove_file(&path).with_context(|| format!("remove login group {}", path.display()))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("web-distributor: {error:#}");
            ExitCode::FAILURE
        }
    }
}
