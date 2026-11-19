use std::fs::File;
use std::path::Path;

use anyhow::Result;

/// Opens a block device for a restore: exclusive, checked against mounts and
/// holders, with its size. The one place that decides a device is safe to overwrite.
pub fn open_device(path: &Path) -> Result<(File, u64)> {
    #[cfg(debug_assertions)]
    if std::env::var("LODE_TEST_DEVICE_IS_FILE").is_ok_and(|v| v == "1") && path.is_file() {
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        let size = file.metadata()?.len();
        return Ok((file, size));
    }
    open_checked(path)
}

#[cfg(not(target_os = "linux"))]
fn open_checked(_path: &Path) -> Result<(File, u64)> {
    anyhow::bail!("--to-device works on Linux only")
}

#[cfg(target_os = "linux")]
fn open_checked(path: &Path) -> Result<(File, u64)> {
    use std::io::ErrorKind;

    use anyhow::{Context, bail};
    use lodestore_format::Error;
    use lodestore_format::fs::raw::open_block_device;

    let opened = match open_block_device(path) {
        Ok(opened) => opened,
        Err(Error::Io(e)) if e.kind() == ErrorKind::ResourceBusy => {
            let why = in_use(path)?.unwrap_or_else(|| {
                format!(
                    "{} is in use: mounted, an active swap or held by another device",
                    path.display()
                )
            });
            bail!(why);
        }
        Err(Error::Io(e)) => {
            return Err(e).with_context(|| format!("cannot open {}", path.display()));
        }
        Err(e) => return Err(e.into()),
    };
    // Checked again with the device held, so nothing can be mounted between the check and the write.
    if let Some(why) = in_use(path)? {
        bail!(why);
    }
    Ok(opened)
}

#[cfg(target_os = "linux")]
fn in_use(path: &Path) -> Result<Option<String>> {
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;

    use anyhow::Context;

    const MAX_MOUNTINFO: u64 = 4 << 20;

    let canonical =
        fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    let name = canonical
        .file_name()
        .context("the device has no name")?
        .to_owned();
    let class = PathBuf::from("/sys/class/block").join(&name);
    let unverifiable = |what: &str, e: std::io::Error| {
        anyhow::anyhow!(
            "cannot check whether {} is in use ({what}: {e})",
            path.display()
        )
    };
    let numbers = fs::read_to_string(class.join("dev")).map_err(|e| unverifiable("sysfs", e))?;
    let sys = fs::canonicalize(PathBuf::from("/sys/dev/block").join(numbers.trim()))
        .map_err(|e| unverifiable("sysfs", e))?;

    let mut raw = Vec::new();
    fs::File::open("/proc/self/mountinfo")
        .and_then(|f| f.take(MAX_MOUNTINFO).read_to_end(&mut raw))
        .map_err(|e| unverifiable("mountinfo", e))?;
    for line in String::from_utf8_lossy(&raw).lines() {
        let mut fields = line.split(' ');
        let (Some(numbers), Some(mountpoint)) = (fields.nth(2), fields.nth(1)) else {
            continue;
        };
        // Mounts without a block device behind them (tmpfs, btrfs subvolumes) have no entry.
        let Ok(mounted) = fs::canonicalize(PathBuf::from("/sys/dev/block").join(numbers)) else {
            continue;
        };
        if mounted.starts_with(&sys) {
            return Ok(Some(format!(
                "{} or one of its partitions is mounted at {mountpoint}",
                path.display()
            )));
        }
    }

    let holders = fs::read_dir(class.join("holders")).map_err(|e| unverifiable("holders", e))?;
    if let Some(holder) = holders.flatten().next() {
        return Ok(Some(format!(
            "{} is held by {}",
            path.display(),
            holder.file_name().to_string_lossy()
        )));
    }
    Ok(None)
}
