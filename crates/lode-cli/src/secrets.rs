use std::io::IsTerminal;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use lodestore_format::keys::{RecoveryKey, looks_like_recovery_key};
use zeroize::Zeroizing;

use crate::cli::{SecretArgs, UnlockError};

pub enum Secret {
    Password(Zeroizing<Vec<u8>>),
    Recovery(RecoveryKey),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Unlock an existing repository: a password or a recovery key.
    Open,
    /// Choose the password of a new repository.
    Init,
}

/// One input channel for both secrets: a value that starts with `lode-rk1-`
/// is a recovery key, anything else is the password. Priority: `--password-file`
/// or `--password-command` (mutually exclusive), `LODE_PASSWORD_FILE`,
/// `LODE_PASSWORD`, the profile's `password_file` then `password_command`, then
/// a prompt on a terminal.
pub fn resolve_secret(args: &SecretArgs, purpose: Purpose) -> Result<Secret> {
    let env_file = std::env::var_os("LODE_PASSWORD_FILE").filter(|p| !p.is_empty());
    let env_value = std::env::var_os("LODE_PASSWORD").filter(|p| !p.is_empty());
    let raw = if let Some(path) = &args.password_file {
        read_file(path, "--password-file")?
    } else if let Some(cmd) = &args.password_command {
        run_command(cmd, "--password-command")?
    } else if let Some(path) = env_file {
        read_file(Path::new(&path), "LODE_PASSWORD_FILE")?
    } else if let Some(value) = env_value {
        Zeroizing::new(value.into_vec())
    } else if let Some(path) = &args.profile_password_file {
        read_file(path, "profile password_file")?
    } else if let Some(cmd) = &args.profile_password_command {
        run_command(cmd, "profile password_command")?
    } else if std::io::stdin().is_terminal() {
        prompt(purpose)?
    } else {
        bail!(
            "no repository password: set LODE_PASSWORD or LODE_PASSWORD_FILE, pass --password-file or --password-command, or run on a terminal to be prompted"
        );
    };
    classify(raw, purpose)
}

/// The password for `key add` and `key passwd`: read from `file`, or typed
/// twice on a terminal. A value shaped like a recovery key is refused, since
/// it would be read back as one.
pub fn resolve_new_password(file: Option<&Path>) -> Result<Zeroizing<Vec<u8>>> {
    let raw = if let Some(path) = file {
        read_file(path, "--new-password-file")?
    } else if std::io::stdin().is_terminal() {
        prompt(Purpose::Init)?
    } else {
        bail!(
            "no new password: pass --new-password-file <PATH> or run on a terminal to be prompted"
        );
    };
    match classify(raw, Purpose::Init)? {
        Secret::Password(password) => Ok(password),
        Secret::Recovery(_) => unreachable!("an Init secret is never a recovery key"),
    }
}

fn classify(raw: Zeroizing<Vec<u8>>, purpose: Purpose) -> Result<Secret> {
    if let Ok(text) = std::str::from_utf8(&raw)
        && looks_like_recovery_key(text)
    {
        if purpose == Purpose::Init {
            bail!("a recovery key cannot be the password of a new repository");
        }
        let key = RecoveryKey::parse(text).map_err(|err| UnlockError(err.to_string()))?;
        return Ok(Secret::Recovery(key));
    }
    Ok(Secret::Password(raw))
}

fn read_file(path: &Path, what: &str) -> Result<Zeroizing<Vec<u8>>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("cannot read {what} {}", path.display()))?;
    let bytes = strip_newline(Zeroizing::new(bytes));
    if bytes.is_empty() {
        bail!("{what} {} is empty", path.display());
    }
    Ok(bytes)
}

/// The command's own stderr and stdin stay attached to the terminal so it can
/// ask for a passphrase; its stdout is the secret and is never echoed.
fn run_command(cmd: &str, what: &str) -> Result<Zeroizing<Vec<u8>>> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("cannot run {what}"))?;
    let stdout = Zeroizing::new(out.stdout);
    match out.status.code() {
        Some(0) => {}
        Some(code) => bail!("{what} failed with exit code {code}"),
        None => bail!("{what} was terminated by a signal"),
    }
    let bytes = strip_newline(stdout);
    if bytes.is_empty() {
        bail!("{what} printed nothing (exit code 0)");
    }
    Ok(bytes)
}

fn prompt(purpose: Purpose) -> Result<Zeroizing<Vec<u8>>> {
    let ask = |text: &str| -> Result<Zeroizing<Vec<u8>>> {
        let entered =
            Zeroizing::new(rpassword::prompt_password(text).context("cannot read the password")?);
        if entered.is_empty() {
            bail!("the password is empty");
        }
        Ok(Zeroizing::new(entered.as_bytes().to_vec()))
    };
    match purpose {
        Purpose::Open => ask("Repository password or recovery key: "),
        Purpose::Init => {
            let first = ask("New repository password: ")?;
            if first != ask("Repeat the password: ")? {
                bail!("the passwords do not match");
            }
            Ok(first)
        }
    }
}

/// Drops one trailing `\n` or `\r\n`, as an editor or `echo` leaves it.
fn strip_newline(mut bytes: Zeroizing<Vec<u8>>) -> Zeroizing<Vec<u8>> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_newline_drops_only_one_terminator() {
        let strip = |s: &[u8]| strip_newline(Zeroizing::new(s.to_vec())).to_vec();
        assert_eq!(strip(b"pw\n"), b"pw");
        assert_eq!(strip(b"pw\r\n"), b"pw");
        assert_eq!(strip(b"pw\n\n"), b"pw\n");
        assert_eq!(strip(b"pw"), b"pw");
        assert_eq!(strip(b"p\rw"), b"p\rw");
    }
}
