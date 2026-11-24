use anyhow::{bail, ensure, Context, Result};
use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{fchown, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

pub fn lock_path(config: &Path) -> PathBuf {
    let mut name = OsString::from(config.as_os_str());
    name.push(".lock");
    PathBuf::from(name)
}

pub fn lock_config(path: &Path) -> Result<File> {
    let path = lock_path(path);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open configuration lock {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&lock).with_context(|| format!("lock {}", path.display()))?;
    Ok(lock)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn stage(
    path: &Path,
    contents: &[u8],
    mode: u32,
    existing: Option<&Metadata>,
) -> Result<NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix(".web-distributor-")
        .tempfile_in(parent(path))
        .with_context(|| format!("stage {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write staged file for {}", path.display()))?;
    if let Some(existing) = existing {
        let current = file.as_file().metadata()?;
        let uid = (current.uid() != existing.uid()).then(|| existing.uid());
        let gid = (current.gid() != existing.gid()).then(|| existing.gid());
        if uid.is_some() || gid.is_some() {
            fchown(file.as_file(), uid, gid)
                .with_context(|| format!("preserve ownership of {}", path.display()))?;
        }
    }
    file.as_file()
        .set_permissions(Permissions::from_mode(mode))
        .with_context(|| format!("set permissions for {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("sync staged file for {}", path.display()))?;
    Ok(file)
}

fn replace(file: NamedTempFile, path: &Path) -> Result<()> {
    file.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn regular_file_metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "{} is not a regular file",
                path.display()
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read metadata for {}", path.display())),
    }
}

fn canonical_parent_path(path: &Path) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "expected a filename"))?;
    Ok(parent(path).canonicalize()?.join(name))
}

pub fn publish(
    config_path: &Path,
    login_dir: &Path,
    output: &Path,
    rendered: &str,
    source: Option<(&Path, &str, u32)>,
) -> Result<()> {
    fs::create_dir_all(parent(output))
        .with_context(|| format!("create output directory {}", parent(output).display()))?;
    let output_path = canonical_parent_path(output)
        .with_context(|| format!("resolve output path {}", output.display()))?;
    let login_path = login_dir
        .canonicalize()
        .or_else(|_| canonical_parent_path(login_dir));
    let login_overlap = match login_path {
        Ok(path) => output_path.starts_with(path),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolve login directory {}", login_dir.display()))
        }
    };
    ensure!(
        output_path != canonical_parent_path(config_path)?
            && output_path != canonical_parent_path(&lock_path(config_path))?
            && !login_overlap,
        "output {} overlaps with the configuration or login groups",
        output.display()
    );

    // Verify the output is replaceable and stage it before changing any source data.
    let output_metadata = regular_file_metadata(output)?;
    let staged_output = stage(output, rendered.as_bytes(), 0o640, output_metadata.as_ref())?;
    let Some((source, contents, mode)) = source else {
        return replace(staged_output, output);
    };

    let source_metadata = regular_file_metadata(source)?;
    let previous = source_metadata
        .as_ref()
        .map(|metadata| {
            let bytes = fs::read(source).with_context(|| format!("read {}", source.display()))?;
            stage(
                source,
                &bytes,
                metadata.permissions().mode() & 0o777,
                Some(metadata),
            )
        })
        .transpose()?;
    let mode = source_metadata
        .as_ref()
        .filter(|_| source == config_path)
        .map_or(mode, |metadata| metadata.permissions().mode() & 0o777);
    let staged_source = stage(source, contents.as_bytes(), mode, source_metadata.as_ref())?;
    replace(staged_source, source)?;
    if let Err(error) = replace(staged_output, output) {
        let recovery = match previous {
            Some(previous) => replace(previous, source),
            None => fs::remove_file(source).context("remove newly created source file"),
        };
        if let Err(recovery) = recovery {
            bail!(
                "{error:#}. Restore {} failed because {recovery:#}",
                source.display()
            );
        }
        return Err(error);
    }
    Ok(())
}

pub fn create_login_group(path: &Path) -> Result<()> {
    fs::create_dir_all(parent(path))
        .with_context(|| format!("create login directory {}", parent(path).display()))?;
    stage(path, b"", 0o600, None)?
        .persist_noclobber(path)
        .map_err(|err| err.error)
        .with_context(|| format!("create login group {}", path.display()))?;
    Ok(())
}
