//! An OpenSSH sshd in a Docker container, built from a Debian base image that
//! is pinned by digest. Four chrooted SFTP users share one repository root:
//! `srv` without limits, `agent` with the deny-list and `-u 0222` the agent
//! gets in production, `wide` with `-u 0222` only, and `nolink`, an agent whose
//! server does not offer the hardlink request.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub const SSHD_BASE_IMAGE: &str =
    "debian:13-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2";

const DOCKERFILE: &str = include_str!("../sshd/Dockerfile");
const SSHD_CONFIG: &str = include_str!("../sshd/sshd_config");
const ENTRY_SH: &str = include_str!("../sshd/entry.sh");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshUser {
    Srv,
    Agent,
    AgentEcdsa,
    Wide,
    NoLink,
}

impl SshUser {
    /// The account name on the server.
    pub fn login(self) -> &'static str {
        match self {
            SshUser::Srv => "srv",
            SshUser::Agent | SshUser::AgentEcdsa => "agent",
            SshUser::Wide => "wide",
            SshUser::NoLink => "nolink",
        }
    }

    fn key_name(self) -> &'static str {
        match self {
            SshUser::Srv => "srv",
            SshUser::Agent => "agent",
            SshUser::AgentEcdsa => "agent_ecdsa",
            SshUser::Wide => "wide",
            SshUser::NoLink => "nolink",
        }
    }
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("sshd tests need Docker: `docker` was not found");
    assert!(
        out.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn image_tag() -> String {
    let mut hasher = Sha256::new();
    for part in [SSHD_BASE_IMAGE, DOCKERFILE, SSHD_CONFIG, ENTRY_SH] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = data_encoding::HEXLOWER.encode(&hasher.finalize());
    format!("lodestore-test-sshd:{}", &digest[..16])
}

fn ensure_image() -> String {
    let tag = image_tag();
    let present = Command::new("docker")
        .args(["image", "inspect", &tag])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("sshd tests need Docker: `docker` was not found")
        .success();
    if !present {
        let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("sshd");
        let base = format!("BASE={SSHD_BASE_IMAGE}");
        docker(&[
            "build",
            "--build-arg",
            &base,
            "-t",
            &tag,
            context.to_str().expect("utf-8 path"),
        ]);
    }
    tag
}

fn keygen(kind: &[&str], path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-N", "", "-C", "lodestore-test"])
        .args(kind)
        .arg("-f")
        .arg(path)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
}

pub struct SshdServer {
    container: String,
    port: u16,
    keys: tempfile::TempDir,
}

impl SshdServer {
    pub async fn start() -> Self {
        let tag = ensure_image();
        let keys = tempfile::tempdir().expect("key directory");
        let public = keys.path().join("pub");
        std::fs::create_dir(&public).expect("public key directory");
        for user in [SshUser::Srv, SshUser::Agent, SshUser::Wide, SshUser::NoLink] {
            keygen(&["-t", "ed25519"], &keys.path().join(user.key_name()));
        }
        keygen(
            &["-t", "ecdsa", "-b", "256"],
            &keys.path().join(SshUser::AgentEcdsa.key_name()),
        );
        for user in [
            SshUser::Srv,
            SshUser::Agent,
            SshUser::AgentEcdsa,
            SshUser::Wide,
            SshUser::NoLink,
        ] {
            let name = user.key_name();
            std::fs::copy(
                keys.path().join(format!("{name}.pub")),
                public.join(format!("{name}.pub")),
            )
            .expect("copy the public key");
        }

        let mount = format!("{}:/keys:ro", public.display());
        let container = docker(&[
            "run",
            "-d",
            "--rm",
            "-p",
            "127.0.0.1::22",
            "-v",
            &mount,
            &tag,
        ])
        .trim()
        .to_owned();
        let mapped = docker(&["port", &container, "22/tcp"]);
        let port = mapped
            .lines()
            .find_map(|l| l.rsplit(':').next()?.trim().parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no mapped port in {mapped:?}"));
        let server = SshdServer {
            container,
            port,
            keys,
        };
        server.wait_ready().await;
        server
    }

    async fn wait_ready(&self) {
        use tokio::io::AsyncReadExt as _;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await {
                let mut banner = [0u8; 8];
                let read =
                    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut banner))
                        .await;
                if matches!(read, Ok(Ok(_))) && banner.starts_with(b"SSH-2.0") {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "sshd did not answer with a banner"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// The server's ed25519 host key as an OpenSSH public key line.
    pub fn host_key(&self) -> String {
        docker(&[
            "exec",
            &self.container,
            "cat",
            "/etc/ssh/ssh_host_ed25519_key.pub",
        ])
        .trim()
        .to_owned()
    }

    /// The OpenSSH private key text of a test user.
    pub fn private_key(&self, user: SshUser) -> String {
        std::fs::read_to_string(self.keys.path().join(user.key_name()))
            .expect("read the private key")
    }

    /// The repository root as the chrooted users see it.
    pub fn root(&self) -> &'static str {
        "/data"
    }

    /// The sshd and sftp-server log, at VERBOSE level.
    pub fn log(&self) -> String {
        docker(&["exec", &self.container, "cat", "/var/log/sshd.log"])
    }

    /// Replaces the ed25519 host key and makes sshd load it.
    pub fn rotate_host_key(&self) {
        docker(&[
            "exec",
            &self.container,
            "sh",
            "-c",
            "rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub \
             && ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key \
             && kill -HUP 1",
        ]);
    }

    /// The container id, for `docker exec` in tests.
    pub fn container(&self) -> &str {
        &self.container
    }
}

impl Drop for SshdServer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_env_matches_constants() {
        crate::assert_pinned("SSHD_BASE_IMAGE", SSHD_BASE_IMAGE);
    }

    #[test]
    fn base_image_is_pinned_by_digest() {
        assert!(SSHD_BASE_IMAGE.contains("@sha256:"));
    }
}
