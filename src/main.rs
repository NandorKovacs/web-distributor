use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, create_dir_all, read_dir, rename};
use std::io::ErrorKind;
use std::path::Path;
use std::process::exit;
use std::time::SystemTime;

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Cli {
    #[arg(short, long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Generate {},
    Add {
        domain: String,
        target: String,

        #[arg(short, long)]
        force: bool,
    },
    Remove {
        domain: String,
    },
    List {},
    LoginGroup {
        #[command(subcommand)]
        pwcommands: PasswordCommands,
    },
}

#[derive(Subcommand, Debug)]
enum PasswordCommands {
    Create {
        login_group: String,
    },
    Remove {
        login_group: String,
    },
    List {},
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

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    home: String,
    acme_redirect_configs: String,
    routes: HashMap<String, String>,
    login_groups: HashMap<String, String>,
}

fn read_config(config_path: &Path) -> Config {
    match fs::read_to_string(&config_path) {
        Ok(config_string) => toml::from_str(&config_string).unwrap(),
        Err(e) => match e.kind() {
            ErrorKind::NotFound => {
                let config = Config {
                    home: "/etc/web-distributor".to_string(),
                    acme_redirect_configs: "/etc/acme-redirect.d".to_string(),
                    routes: HashMap::new(),
                    login_groups: HashMap::new(),
                };
                write_config(&config, &config_path);
                config
            }
            _ => {
                panic!();
            }
        },
    }
}

fn nginx_proxy_build(from: &str, to: &str, login_group_path: Option<String>) -> String {
    match login_group_path {
        Some(lg) => format!(
            include_str!("nginx.conf"),
            from = from,
            to = to,
            login_group = format!(
                "auth_basic \"login\";
        auth_basic_user_file {};",
                lg
            )
        ),
        None => format!(
            include_str!("nginx.conf"),
            from = from,
            to = to,
            login_group = ""
        ),
    }
}

fn acme_redirect_config_build(namespace: &str) -> String {
    format!(include_str!("acme-redirect.conf"), namespace = namespace)
}

fn generate_webserver_configs(config: &Config, timestring: &str) {
    let nginx_folder = Path::new(&config.home).join("nginx");
    let archive = Path::new(&config.home).join(format!("nginx-old-{timestring}"));
    let backup = Path::new(&config.home).join("nginx-old");

    if let Err(e) = rename(&backup, &archive) {
        if e.kind().ne(&ErrorKind::NotFound) {
            panic!("couldn't move {:?} to {:?}", &backup, &archive);
        }
    }

    if let Err(e) = rename(&nginx_folder, &backup) {
        if e.kind().ne(&ErrorKind::NotFound) {
            panic!("couldn't move {:?} to {:?}", &backup, &archive);
        }
    }

    create_dir_all(&nginx_folder).unwrap();

    for (source, target) in &config.routes {
        let access_str = if config.login_groups.contains_key(source) {
            Some(
                Path::new(&config.home)
                    .join(config.login_groups.get(source).unwrap())
                    .to_str()
                    .unwrap()
                    .to_string(),
            )
        } else {
            None
        };
        fs::write(
            nginx_folder.join(format!("{}.nginx", source)),
            nginx_proxy_build(&source, &target, access_str),
        )
        .unwrap();
    }
}

fn generate_acme_redirect_config(config: &Config, timestring: &str) {
    let acme_path: &Path = Path::new(&config.acme_redirect_configs);
    let backup = acme_path.join("web-distributor-old");
    let archive = acme_path.join(format!("web-distributor-old-{timestring}"));

    if let Err(e) = rename(&backup, &archive) {
        if e.kind().ne(&ErrorKind::NotFound) {
            panic!("couldn't move {:?} to {:?}", &backup, &archive);
        }
    }

    create_dir_all(&backup).unwrap();

    let dir_entries = read_dir(acme_path).unwrap();
    for f in dir_entries {
        let entry = f.unwrap();
        if entry
            .file_name()
            .to_str()
            .unwrap()
            .starts_with("web-distributor-old")
            || !entry
                .file_name()
                .to_str()
                .unwrap()
                .starts_with("web-distributor")
        {
            continue;
        }
        rename(
            acme_path.join(entry.file_name()),
            backup.join(entry.file_name()),
        )
        .unwrap();
    }

    for (namespace, _) in &config.routes {
        fs::write(
            acme_path.join(format!("web-distributor.{namespace}.conf")),
            acme_redirect_config_build(&namespace),
        )
        .unwrap();
    }
}

fn write_config(config: &Config, config_path: &Path) {
    fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
}

fn write_passwd(file_string: &str, login_group_path: &Path) {
    fs::write(login_group_path, file_string).unwrap();
}

fn main() {
    let cli = Cli::parse();

    let config_path: &Path = match &cli.config {
        Some(config_str) => Path::new(config_str),
        None => Path::new("/etc/web-distributor.toml"),
    };

    let mut config = read_config(&config_path);

    let login_dir = Path::new(&config.home).join("login_groups");
    create_dir_all(&login_dir).unwrap();

    let login_groups: Vec<String> = read_dir(&login_dir)
        .unwrap()
        .map(|f| f.unwrap().file_name().to_str().unwrap().to_string())
        .collect();

    for login_group in config.login_groups.values() {
        if !login_groups.contains(login_group) {
            write_passwd("", &login_dir.join(login_group));
        }
    }

    match &cli.command {
        Commands::Add {
            domain,
            target,
            force,
        } => {
            if config.routes.contains_key(domain) && !force {
                eprintln!(
                    "There already exists a route from {} to {}. To override, use --force.",
                    domain,
                    config
                        .routes
                        .get(domain)
                        .expect("wtf we just checked if this exists")
                );
                exit(1);
            }

            config.routes.insert(domain.to_string(), target.to_string());
            write_config(&config, &config_path);
        }
        Commands::Remove { domain } => {
            if !config.routes.contains_key(domain) {
                eprintln!("Can't remove {domain}, it doesn't exist.");
                exit(1);
            }
            config.routes.remove(domain);
            write_config(&config, &config_path);
        }
        Commands::Generate {} => {}
        Commands::LoginGroup { pwcommands } => match pwcommands {
            PasswordCommands::Create { login_group } => {
                if login_groups.contains(login_group) {
                    eprintln!("Login group {login_group} already exists.");
                    exit(1);
                }
                write_passwd("", login_dir.join(login_group).as_path());
                exit(0);
            }
            PasswordCommands::Remove { login_group } => {
                if !login_groups.contains(login_group) {
                    eprintln!("Login group {login_group} doesn't exist.");
                    exit(1);
                }
                fs::remove_file(&login_dir.join(login_group)).unwrap();
                config.login_groups.retain(|_, v| v.ne(&login_group));
                write_config(&config, config_path);
            }
            PasswordCommands::Apply {
                domain,
                force,
                login_group,
            } => {
                if !login_groups.contains(login_group) {
                    eprintln!("Login group {login_group} doesn't exist");
                    exit(1);
                }

                if !config.routes.contains_key(domain) {
                    eprintln!("No route with source domain {domain} exists");
                    exit(0);
                }

                if config.login_groups.contains_key(domain) && !force {
                    eprintln!("There already exists an active login group for {domain}. To override, use --force.",);
                    exit(1);
                }
                config
                    .login_groups
                    .insert(domain.to_string(), login_group.to_string());
                write_config(&config, config_path);
                // here regenerate
            }
            PasswordCommands::AddLogin {
                name,
                password,
                force,
                login_group,
            } => {
                if !login_groups.contains(login_group) {
                    eprintln!("Login group {login_group} doesn't exist");
                    exit(1);
                }

                let pwdfile = fs::read_to_string(login_dir.join(login_group)).unwrap();
                if !force {
                    for f in pwdfile.lines() {
                        if f.starts_with(name) {
                            eprintln!("There already exists a login for {name} in login group {login_group}. To override, use --force.");
                            exit(0);
                        }
                    }
                }
                let password = match password {
                    Some(p) => p.to_string(),
                    None => rpassword::prompt_password("Enter password:").unwrap(),
                };
                let pwdfile = pwdfile
                    .lines()
                    .filter(|f| !f.starts_with(name))
                    .collect::<Vec<&str>>()
                    .join("\n");
                let pwdfile = format!(
                    "{}\n{}",
                    pwdfile,
                    format!("{}:{}", name, bcrypt::hash(password, 5).unwrap())
                );
                write_passwd(&pwdfile, &login_dir.join(login_group));
                exit(0);
            }
            PasswordCommands::RevokeLogin { name, login_group } => {
                if !login_groups.contains(login_group) {
                    eprintln!("Login group {login_group} doesn't exist");
                    exit(1);
                }

                let pwdfile = fs::read_to_string(login_dir.join(login_group)).unwrap();

                let mut x = false;
                for line in pwdfile.lines() {
                    if line.starts_with(name) {
                        x = true;
                        break;
                    }
                }
                if !x {
                    eprintln!("No login with {name} exists in login group {login_group}");
                    exit(1);
                }

                let pwdfile = pwdfile
                    .lines()
                    .filter(|f| !f.starts_with(name))
                    .collect::<Vec<&str>>()
                    .join("\n");
                write_passwd(&pwdfile, &login_dir.join(login_group));
                exit(0);
            }
            PasswordCommands::List {} => {
                for x in login_groups {
                    println!("{}", x);
                }
                exit(0);
            }
            PasswordCommands::Disable { domain } => {
                if !config.login_groups.contains_key(domain) {
                    eprintln!("No login group is active for {domain}");
                    exit(1);
                }

                config.login_groups.remove(domain);
                write_config(&config, &config_path);
                // here regenerate
            }
        },
        Commands::List {} => {
            for (source, destination) in &config.routes {
                let lg = match config.login_groups.get(source) {
                    Some(s) => format!(", login group: {}", s),
                    None => "".to_string(),
                };
                println!("{source} => {destination}{}", lg);
            }
            exit(0);
        }
    }

    let timestring = format!(
        "{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("couldn't get unix time")
            .as_secs_f64()
            .to_string()
    );

    generate_webserver_configs(&config, &timestring);

    generate_acme_redirect_config(&config, &timestring);
}
