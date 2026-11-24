use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};

fn default_home() -> PathBuf {
    PathBuf::from("/etc/web-distributor")
}

#[derive(Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_home")]
    pub home: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acme_redirect_configs: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub certificate_only: BTreeSet<String>,
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
    #[serde(default)]
    pub login_groups: BTreeMap<String, String>,
    #[serde(default)]
    pub forward_auth: BTreeMap<String, String>,
    #[serde(default)]
    pub grpc_prefix: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home: default_home(),
            acme_redirect_configs: None,
            certificate_only: BTreeSet::new(),
            routes: BTreeMap::new(),
            login_groups: BTreeMap::new(),
            forward_auth: BTreeMap::new(),
            grpc_prefix: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn read(path: &Path) -> Result<(Self, bool)> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok((
                toml::from_str(&contents)
                    .with_context(|| format!("parse configuration {}", path.display()))?,
                true,
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok((Self::default(), false)),
            Err(error) => {
                Err(error).with_context(|| format!("read configuration {}", path.display()))
            }
        }
    }
}

fn valid_host(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub fn validate_domain(domain: &str) -> Result<()> {
    if !valid_host(domain) || domain.ends_with('.') {
        bail!("invalid site hostname {domain:?}");
    }
    Ok(())
}

pub fn validate_target(target: &str) -> Result<()> {
    let (valid, port) = if let Some(ipv6) = target.strip_prefix('[') {
        match ipv6.split_once(']') {
            Some((host, "")) => (host.parse::<Ipv6Addr>().is_ok(), None),
            Some((host, rest)) => (
                host.parse::<Ipv6Addr>().is_ok(),
                rest.strip_prefix(':').or(Some("")),
            ),
            None => (false, None),
        }
    } else if let Some((host, port)) = target.split_once(':') {
        (valid_host(host), Some(port))
    } else {
        (valid_host(target), None)
    };
    let valid_port = port.is_none_or(|port| {
        port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|number| number > 0)
    });
    if !valid || !valid_port {
        bail!("invalid upstream {target:?}, expected a host or host:port");
    }
    Ok(())
}

pub fn grpc_path_pattern(prefix: &str) -> Result<String> {
    if !prefix.starts_with('/')
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._~!$&'()+,;=:@-".contains(&b))
    {
        bail!("invalid gRPC prefix {prefix:?}, expected a literal URL path prefix");
    }
    let mut pattern = String::from("^");
    for character in prefix.chars() {
        if ".$()+".contains(character) {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    Ok(pattern)
}
