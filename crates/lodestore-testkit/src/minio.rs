//! A MinIO server started as a child process from a release binary that is
//! downloaded once, checked against a pinned sha256 and cached under
//! `target/test-bin`. `mc` drives the admin side (buckets, users, locks).

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub const MINIO_RELEASE: &str = "RELEASE.2025-09-07T16-13-09Z";
pub const MC_RELEASE: &str = "RELEASE.2025-08-13T08-35-41Z";

struct Asset {
    url: String,
    file: String,
    sha256: &'static str,
}

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "aarch64") => "darwin-arm64",
        (os, arch) => panic!("no pinned MinIO binary for {os}/{arch}"),
    }
}

fn minio_asset() -> Asset {
    let p = platform();
    let sha256 = match p {
        "linux-amd64" => "7c5bd8512c6e966455b1d198209358b2d191c77a83ab377c4073281065fb855f",
        "linux-arm64" => "5c83cd2cf151717ba0243f73e1c7802ff36e272b67144bdd7f1f7d684fd6f03d",
        _ => "7c3b3039b76e55a1b80935848ed83998d5e8d317374f87851f46a019ff5c0aa4",
    };
    let file = format!("minio.{p}.{MINIO_RELEASE}");
    Asset {
        url: format!("https://github.com/minio/minio/releases/download/{MINIO_RELEASE}/{file}"),
        file,
        sha256,
    }
}

fn mc_asset() -> Asset {
    let p = platform();
    let sha256 = match p {
        "linux-amd64" => "01f866e9c5f9b87c2b09116fa5d7c06695b106242d829a8bb32990c00312e891",
        "linux-arm64" => "14c8c9616cfce4636add161304353244e8de383b2e2752c0e9dad01d4c27c12c",
        _ => "a877fd0c183409da9f20f9d6e1811987298bbbca1aa03428eebdffba79fb9445",
    };
    let file = format!("mc.{p}.{MC_RELEASE}");
    Asset {
        url: format!("https://github.com/minio/mc/releases/download/{MC_RELEASE}/{file}"),
        file,
        sha256,
    }
}

fn cache_dir() -> PathBuf {
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(target).join("test-bin");
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-bin")
        .components()
        .collect()
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(data_encoding::HEXLOWER.encode(&hasher.finalize()))
}

/// Panics, and deletes the file, unless it has the pinned digest.
fn verify(path: &Path, asset: &Asset) {
    let actual = sha256_of(path).expect("hash the downloaded binary");
    if actual != asset.sha256 {
        let _ = std::fs::remove_file(path);
        panic!(
            "sha256 mismatch for {}: expected {}, got {actual}",
            asset.file, asset.sha256
        );
    }
}

async fn fetch(asset: &Asset) -> PathBuf {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).expect("create the test-bin cache");
    let target = dir.join(&asset.file);
    if target.exists() {
        verify(&target, asset);
        return target;
    }

    let tmp = dir.join(format!(".{}.{}.tmp", asset.file, std::process::id()));
    let mut resp = reqwest::get(&asset.url)
        .await
        .and_then(|r| r.error_for_status())
        .unwrap_or_else(|e| panic!("download {}: {e}", asset.url));
    let mut out = std::fs::File::create(&tmp).expect("create the temporary binary");
    while let Some(chunk) = resp.chunk().await.expect("download the binary") {
        out.write_all(&chunk).expect("write the binary");
    }
    out.sync_all().expect("sync the binary");
    drop(out);
    verify(&tmp, asset);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::rename(&tmp, &target).expect("move the binary into the cache");
    target
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("find a free port")
        .port()
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("os randomness");
    data_encoding::HEXLOWER.encode(&buf)
}

pub struct MinioServer {
    child: Child,
    mc: PathBuf,
    mc_config: tempfile::TempDir,
    _data: tempfile::TempDir,
    endpoint: String,
    root_user: String,
    root_password: String,
}

impl MinioServer {
    pub async fn start() -> Self {
        let minio = fetch(&minio_asset()).await;
        let mc = fetch(&mc_asset()).await;

        let data = tempfile::tempdir().expect("data dir");
        let mc_config = tempfile::tempdir().expect("mc config dir");
        let (port, console) = (free_port(), free_port());
        let (root_user, root_password) = (random_hex(8), random_hex(16));
        let child = Command::new(&minio)
            .arg("server")
            .arg(data.path())
            .args(["--address", &format!("127.0.0.1:{port}")])
            .args(["--console-address", &format!("127.0.0.1:{console}")])
            .env("MINIO_ROOT_USER", &root_user)
            .env("MINIO_ROOT_PASSWORD", &root_password)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start minio");
        let mut server = MinioServer {
            child,
            mc,
            mc_config,
            _data: data,
            endpoint: format!("http://127.0.0.1:{port}"),
            root_user,
            root_password,
        };
        server.wait_ready().await;
        let (endpoint, user, password) = (
            server.endpoint.clone(),
            server.root_user.clone(),
            server.root_password.clone(),
        );
        server
            .mc(&["alias", "set", "lode", &endpoint, &user, &password])
            .await
            .expect("mc alias set");
        server
    }

    async fn wait_ready(&mut self) {
        let url = format!("{}/minio/health/live", self.endpoint);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("minio exited during startup: {status}");
            }
            if let Ok(r) = reqwest::get(&url).await
                && r.status().is_success()
            {
                return;
            }
            assert!(Instant::now() < deadline, "minio did not become ready");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Runs `mc` against this server; the error carries stdout and stderr.
    async fn mc(&self, args: &[&str]) -> Result<String, String> {
        let out = tokio::process::Command::new(&self.mc)
            .arg("--config-dir")
            .arg(self.mc_config.path())
            .arg("--no-color")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(format!("{stdout}{}", String::from_utf8_lossy(&out.stderr)))
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn root_credentials(&self) -> (String, String) {
        (self.root_user.clone(), self.root_password.clone())
    }

    pub async fn create_bucket(&self, name: &str) {
        self.mc(&["mb", &format!("lode/{name}")])
            .await
            .expect("mc mb");
    }

    /// A bucket with Object Lock and a default COMPLIANCE retention of a day.
    pub async fn create_locked_bucket(&self, name: &str) {
        let target = format!("lode/{name}");
        self.mc(&["mb", "--with-lock", &target])
            .await
            .expect("mc mb --with-lock");
        self.mc(&["retention", "set", "--default", "COMPLIANCE", "1d", &target])
            .await
            .expect("mc retention set");
    }

    /// A user that can add objects under `packs/`, `index/` and `snapshots/`,
    /// read `config` and `index/`, and list `index/`, but never delete.
    pub async fn create_write_only_user(&self, bucket: &str) -> (String, String) {
        let (access_key, secret_key) = (random_hex(8), random_hex(16));
        let arn = |path: &str| format!("arn:aws:s3:::{bucket}/{path}");
        let policy = serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [
                {"Effect": "Allow", "Action": ["s3:PutObject"],
                 "Resource": [arn("packs/*"), arn("index/*"), arn("snapshots/*")]},
                {"Effect": "Allow", "Action": ["s3:GetObject"],
                 "Resource": [arn("config"), arn("index/*")]},
                {"Effect": "Allow", "Action": ["s3:ListBucket"],
                 "Resource": [format!("arn:aws:s3:::{bucket}")],
                 "Condition": {"StringLike": {"s3:prefix": ["index/*", "index/"]}}},
            ],
        });
        let name = format!("write-only-{access_key}");
        let file = self.mc_config.path().join(format!("{name}.json"));
        std::fs::write(&file, policy.to_string()).expect("write the policy");
        let file = file.to_str().expect("utf-8 path");
        self.mc(&["admin", "user", "add", "lode", &access_key, &secret_key])
            .await
            .expect("mc admin user add");
        self.mc(&["admin", "policy", "create", "lode", &name, file])
            .await
            .expect("mc admin policy create");
        self.mc(&[
            "admin",
            "policy",
            "attach",
            "lode",
            &name,
            "--user",
            &access_key,
        ])
        .await
        .expect("mc admin policy attach");
        (access_key, secret_key)
    }

    /// Version ids of the object, newest first; delete markers are skipped.
    pub async fn object_versions(&self, bucket: &str, key: &str) -> Vec<String> {
        let out = self
            .mc(&[
                "ls",
                "--versions",
                "--json",
                &format!("lode/{bucket}/{key}"),
            ])
            .await
            .expect("mc ls --versions");
        let name = key.rsplit('/').next().unwrap_or(key);
        out.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| {
                v["key"].as_str() == Some(name) && v["isDeleteMarker"].as_bool() != Some(true)
            })
            .filter_map(|v| v["versionId"].as_str().map(str::to_owned))
            .collect()
    }

    pub async fn delete_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), String> {
        self.mc(&[
            "rm",
            "--version-id",
            version_id,
            &format!("lode/{bucket}/{key}"),
        ])
        .await
        .map(|_| ())
    }
}

impl Drop for MinioServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
