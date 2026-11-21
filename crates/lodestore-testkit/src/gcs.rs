//! A fake-gcs-server started as a child process from a release archive that is
//! downloaded once, checked against a pinned sha256 before it is unpacked and
//! cached under `target/test-bin`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::minio::{cache_dir, download, free_port, sha256_of};

pub const FAKE_GCS_RELEASE: &str = "v1.56.1";

const FAKE_GCS_SHA256_LINUX_AMD64: &str =
    "0f2f9f6884417acd5cc27126a9aa237ccc627372a4aa5008b399641de6db9469";
const FAKE_GCS_SHA256_LINUX_ARM64: &str =
    "89a4b887a50834ce5844f47f9938a1203a89330e660b2af7851b858fa489fa96";
const FAKE_GCS_SHA256_DARWIN_ARM64: &str =
    "039d7a520869115b9bc1b354f73612d2dad0aa27b22104f111d8c94985007fe1";
const FAKE_GCS_SHA256_DARWIN_AMD64: &str =
    "bbcbb78e57c4a7ad242bc971b70a57f963c8daa616ae649254562fbd7f2bc54d";

fn archive_name() -> (String, &'static str) {
    let (os, arch, sha256) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("Linux", "amd64", FAKE_GCS_SHA256_LINUX_AMD64),
        ("linux", "aarch64") => ("Linux", "arm64", FAKE_GCS_SHA256_LINUX_ARM64),
        ("macos", "aarch64") => ("Darwin", "arm64", FAKE_GCS_SHA256_DARWIN_ARM64),
        ("macos", "x86_64") => ("Darwin", "amd64", FAKE_GCS_SHA256_DARWIN_AMD64),
        (os, arch) => panic!("no pinned fake-gcs-server binary for {os}/{arch}"),
    };
    let version = FAKE_GCS_RELEASE.trim_start_matches('v');
    (
        format!("fake-gcs-server_{version}_{os}_{arch}.tar.gz"),
        sha256,
    )
}

async fn fetch() -> PathBuf {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).expect("create the test-bin cache");
    let target = dir.join(format!("fake-gcs-server-{FAKE_GCS_RELEASE}"));
    if target.exists() {
        return target;
    }

    let (file, sha256) = archive_name();
    let archive = dir.join(format!(".{file}.{}.tmp", std::process::id()));
    download(
        &format!(
            "https://github.com/fsouza/fake-gcs-server/releases/download/{FAKE_GCS_RELEASE}/{file}"
        ),
        &archive,
    )
    .await;
    let actual = sha256_of(&archive).expect("hash the downloaded archive");
    if actual != sha256 {
        let _ = std::fs::remove_file(&archive);
        panic!("sha256 mismatch for {file}: expected {sha256}, got {actual}");
    }

    let unpacked = tempfile::tempdir_in(&dir).expect("unpack directory");
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(unpacked.path())
        .status()
        .expect("run tar");
    let _ = std::fs::remove_file(&archive);
    assert!(status.success(), "tar failed on {file}");
    let binary = unpacked.path().join("fake-gcs-server");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::rename(&binary, &target).expect("move the binary into the cache");
    target
}

pub struct FakeGcsServer {
    child: Child,
    port: u16,
    client: reqwest::Client,
}

impl FakeGcsServer {
    pub async fn start() -> Self {
        let binary = fetch().await;
        let mut last = String::new();
        for _ in 0..5 {
            let port = free_port();
            match Self::launch(&binary, port, &format!("127.0.0.1:{port}")).await {
                Ok(server) => return server,
                Err(e) => last = e,
            }
        }
        panic!("fake-gcs-server did not start on a free port: {last}");
    }

    /// Listens on `port` but advertises `public_host` in the URLs it hands out,
    /// so a proxy in front of it stays on the path of resumable sessions.
    pub async fn start_with_public_host(port: u16, public_host: &str) -> Self {
        let binary = fetch().await;
        Self::launch(&binary, port, public_host)
            .await
            .unwrap_or_else(|e| panic!("fake-gcs-server did not start on port {port}: {e}"))
    }

    async fn launch(
        binary: &std::path::Path,
        port: u16,
        public_host: &str,
    ) -> Result<Self, String> {
        let child = Command::new(binary)
            .args([
                "-scheme",
                "http",
                "-backend",
                "memory",
                "-host",
                "127.0.0.1",
            ])
            .args(["-port", &port.to_string()])
            .args(["-public-host", public_host])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start fake-gcs-server");
        let mut server = FakeGcsServer {
            child,
            port,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build the client"),
        };
        server.wait_ready().await?;
        Ok(server)
    }

    async fn wait_ready(&mut self) -> Result<(), String> {
        let url = format!("{}/storage/v1/b", self.endpoint());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("fake-gcs-server exited during startup: {status}"));
            }
            if let Ok(r) = self.client.get(&url).send().await
                && r.status().is_success()
            {
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "fake-gcs-server did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub async fn create_bucket(&self, name: &str) {
        let resp = self
            .client
            .post(format!("{}/storage/v1/b", self.endpoint()))
            .header("content-type", "application/json")
            .body(serde_json::json!({ "name": name }).to_string())
            .send()
            .await
            .expect("create the bucket");
        assert!(
            resp.status().is_success(),
            "create bucket {name}: {}",
            resp.status()
        );
    }
}

impl Drop for FakeGcsServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_env_matches_constants() {
        for (key, value) in [
            ("FAKE_GCS_RELEASE", FAKE_GCS_RELEASE),
            ("FAKE_GCS_SHA256_LINUX_AMD64", FAKE_GCS_SHA256_LINUX_AMD64),
            ("FAKE_GCS_SHA256_LINUX_ARM64", FAKE_GCS_SHA256_LINUX_ARM64),
            ("FAKE_GCS_SHA256_DARWIN_ARM64", FAKE_GCS_SHA256_DARWIN_ARM64),
            ("FAKE_GCS_SHA256_DARWIN_AMD64", FAKE_GCS_SHA256_DARWIN_AMD64),
        ] {
            crate::assert_pinned(key, value);
        }
    }
}
