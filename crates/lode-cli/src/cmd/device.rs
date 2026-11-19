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

/// Warns when the block device a backup reads is in use: its image is then
/// crash-consistent at best.
pub fn warn_if_in_use(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device()) {
            return;
        }
        match in_use(path) {
            Ok(Some(why)) => eprintln!(
                "warning: {why}; the image is crash-consistent at best, \
                 and the backup fails if the device size changes"
            ),
            Ok(None) => {}
            Err(e) => eprintln!("warning: {e:#}"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
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
    use std::path::PathBuf;

    use anyhow::Context;

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

    let raw = fs::File::open("/proc/self/mountinfo")
        .and_then(|f| read_capped(f, MAX_MOUNTINFO))
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

#[cfg(any(target_os = "linux", test))]
const MAX_MOUNTINFO: u64 = 4 << 20;

/// Reads all of `source`, or fails when it holds more than `max` bytes: a
/// mount table cut short could hide the mount that makes a device busy.
#[cfg(any(target_os = "linux", test))]
fn read_capped(source: impl std::io::Read, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut raw = Vec::new();
    source.take(max + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > max {
        return Err(std::io::Error::other(format!(
            "it is larger than {max} bytes"
        )));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::{MAX_MOUNTINFO, read_capped};

    #[test]
    fn mount_table_over_the_limit_is_refused() {
        let max = MAX_MOUNTINFO as usize;
        assert_eq!(read_capped(&vec![b'a'; max][..], MAX_MOUNTINFO).unwrap().len(), max);
        assert!(read_capped(&vec![b'a'; max + 1][..], MAX_MOUNTINFO).is_err());
        assert_eq!(read_capped(&b"x"[..], 0).ok(), None);
        assert_eq!(read_capped(&b""[..], 0).unwrap(), b"");
    }
}
