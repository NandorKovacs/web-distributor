use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub type Accounts = BTreeMap<String, String>;

pub fn group_path(directory: &Path, group: &str) -> Result<PathBuf> {
    ensure!(
        !group.starts_with('.')
            && !group.is_empty()
            && group
                .chars()
                .all(|c| !c.is_control() && !matches!(c, '/' | '\\')),
        "invalid login group name {group:?}"
    );
    Ok(directory.join(group))
}

pub fn validate_username(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name != "import"
            && name.chars().all(|c| {
                !c.is_control() && !matches!(c, ':' | '"' | '\\' | '`' | '{' | '}' | '#')
            }),
        "invalid login name {name:?}"
    );
    Ok(())
}

pub fn check_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("find login group {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "login group {} is not a regular file",
        path.display()
    );
    Ok(())
}

pub fn read(path: &Path) -> Result<Accounts> {
    check_file(path)?;
    let contents =
        fs::read_to_string(path).with_context(|| format!("read login group {}", path.display()))?;
    let mut accounts = Accounts::new();
    for (line_number, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let invalid_entry = || {
            anyhow::anyhow!(
                "invalid bcrypt entry in {} on line {}",
                path.display(),
                line_number + 1
            )
        };
        let (name, hash) = line.split_once(':').ok_or_else(invalid_entry)?;
        validate_username(name).with_context(invalid_entry)?;
        // The complete bcrypt encoding has a 7-byte version/cost prefix and 53 encoded bytes.
        ensure!(
            hash.len() == 60
                && matches!(hash.get(..4), Some("$2a$" | "$2b$" | "$2y$"))
                && matches!(
                    hash.get(4..6).and_then(|cost| cost.parse::<u8>().ok()),
                    Some(4..=31)
                )
                && hash.as_bytes()[6] == b'$'
                && hash.as_bytes()[7..]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/')),
            "{}",
            invalid_entry()
        );
        ensure!(
            accounts.insert(name.to_owned(), hash.to_owned()).is_none(),
            "duplicate login {name:?} in {}",
            path.display()
        );
    }
    Ok(accounts)
}

pub fn serialize(accounts: &Accounts) -> String {
    let mut contents = String::new();
    for (name, hash) in accounts {
        contents.push_str(&format!("{name}:{hash}\n"));
    }
    contents
}

pub fn list(directory: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("list login groups in {}", directory.display()))
        }
    };
    let mut groups = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid filename in {}", directory.display()))?;
        if !name.starts_with('.') {
            if group_path(directory, &name).is_err() {
                bail!(
                    "invalid login group filename {name:?} in {}",
                    directory.display()
                );
            }
            groups.push(name);
        }
    }
    groups.sort_unstable();
    Ok(groups)
}
