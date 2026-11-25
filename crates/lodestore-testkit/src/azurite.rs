//! An Azurite blob service started as a child process. The package comes from
//! the committed npm lock (`scripts/azurite`), installed with `npm ci
//! --ignore-scripts` into `target/test-bin`, so every transitive package is
//! pinned by integrity and none runs an install script.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_lc_rs::hmac;
use data_encoding::BASE64;

use crate::minio::{cache_dir, free_port};

pub const AZURITE_VERSION: &str = "3.37.0";
const AZURITE_NPM_INTEGRITY: &str = "sha512-dl2AXMkXtVlX9s3U3ttIQFFlKwe64k/ugSIVNT4V3qX488Ncw988rIHAs6apgUtfEzyqwtFN9MZp0Foq60Htag==";

const PACKAGE_JSON: &str = include_str!("../../../scripts/azurite/package.json");
const PACKAGE_LOCK: &str = include_str!("../../../scripts/azurite/package-lock.json");

const API_VERSION: &str = "2022-11-02";
pub const ACCOUNT_NAME: &str = "devstoreaccount1";
/// The well-known Azurite development key, published in Microsoft's documentation.
pub const ACCOUNT_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

fn install() -> std::path::PathBuf {
    assert!(
        PACKAGE_LOCK.contains(AZURITE_NPM_INTEGRITY),
        "scripts/azurite/package-lock.json does not carry the pinned azurite integrity"
    );
    let out = Command::new("node")
        .arg("--version")
        .output()
        .expect("Azurite needs Node 22 or newer: `node` was not found");
    let version = String::from_utf8_lossy(&out.stdout);
    let major: u32 = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    assert!(
        major >= 22,
        "Azurite needs Node 22 or newer, found {}",
        version.trim()
    );

    let dir = cache_dir();
    std::fs::create_dir_all(&dir).expect("create the test-bin cache");
    let target = dir.join(format!("azurite-{AZURITE_VERSION}"));
    let marker = target.join("node_modules/azurite/package.json");
    let installed = |p: &std::path::Path| {
        std::fs::read_to_string(p)
            .is_ok_and(|s| s.contains(&format!("\"version\": \"{AZURITE_VERSION}\"")))
    };
    if installed(&marker) {
        return target;
    }

    let staging = tempfile::tempdir_in(&dir).expect("staging directory");
    std::fs::write(staging.path().join("package.json"), PACKAGE_JSON).expect("write package.json");
    std::fs::write(staging.path().join("package-lock.json"), PACKAGE_LOCK)
        .expect("write package-lock.json");
    let status = Command::new("npm")
        .args(["ci", "--ignore-scripts", "--no-audit", "--no-fund"])
        .current_dir(staging.path())
        .stdin(Stdio::null())
        .status()
        .expect("run npm");
    assert!(status.success(), "npm ci failed for Azurite");
    assert!(
        installed(&staging.path().join("node_modules/azurite/package.json")),
        "npm ci did not install azurite {AZURITE_VERSION}"
    );
    if installed(&marker) {
        return target;
    }
    let _ = std::fs::remove_dir_all(&target);
    if std::fs::rename(staging.path(), &target).is_err() {
        assert!(
            installed(&marker),
            "move the Azurite install into the cache"
        );
    }
    target
}

fn hmac_base64(key: &[u8], message: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    BASE64.encode(hmac::sign(&key, message.as_bytes()).as_ref())
}

fn account_key() -> Vec<u8> {
    BASE64.decode(ACCOUNT_KEY.as_bytes()).expect("account key")
}

fn percent_encode_signature(sig: &str) -> String {
    sig.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

struct Civil {
    year: i64,
    month: u64,
    day: u64,
    hour: u64,
    minute: u64,
    second: u64,
    weekday: usize,
}

fn civil(unix: u64) -> Civil {
    let days = unix / 86_400;
    let rest = unix % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    let year = yoe + era * 400 + i64::from(month <= 2);
    Civil {
        year,
        month,
        day,
        hour: rest / 3600,
        minute: rest % 3600 / 60,
        second: rest % 60,
        weekday: ((days + 4) % 7) as usize,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

fn rfc1123(unix: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let c = civil(unix);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[c.weekday],
        c.day,
        MONTHS[c.month as usize - 1],
        c.year,
        c.hour,
        c.minute,
        c.second
    )
}

fn iso8601(unix: u64) -> String {
    let c = civil(unix);
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

pub fn iso8601_utc(unix_secs: u64) -> String {
    iso8601(unix_secs)
}

/// The base64 signature of a container service SAS, built from the
/// documented string-to-sign without the code under test: sp, st, se,
/// canonicalized resource, si, sip, spr, sv, sr, snapshot, scope, rscc, rscd,
/// rsce, rscl, rsct.
#[allow(clippy::too_many_arguments)]
pub fn container_sas_signature(
    account_name: &str,
    account_key_b64: &str,
    container: &str,
    permissions: &str,
    start: Option<&str>,
    expiry: &str,
    protocol: Option<&str>,
    version: &str,
) -> String {
    let to_sign = [
        permissions,
        start.unwrap_or(""),
        expiry,
        &format!("/blob/{account_name}/{container}"),
        "",
        "",
        protocol.unwrap_or(""),
        version,
        "c",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ]
    .join("\n");
    let key = BASE64
        .decode(account_key_b64.as_bytes())
        .expect("account key is base64");
    hmac_base64(&key, &to_sign)
}

pub struct AzuriteServer {
    child: Child,
    port: u16,
    _data: tempfile::TempDir,
    client: reqwest::Client,
}

impl AzuriteServer {
    pub async fn start() -> Self {
        let root = install();
        let mut last = String::new();
        for _ in 0..5 {
            match Self::launch(&root).await {
                Ok(server) => return server,
                Err(e) => last = e,
            }
        }
        panic!("azurite did not start on a free port: {last}");
    }

    async fn launch(root: &std::path::Path) -> Result<Self, String> {
        let data = tempfile::tempdir().expect("data dir");
        let port = free_port();
        let child = Command::new(root.join("node_modules/.bin/azurite-blob"))
            .args(["--blobHost", "127.0.0.1", "--blobPort", &port.to_string()])
            .args(["--inMemoryPersistence", "--skipApiVersionCheck", "--silent"])
            .current_dir(data.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start azurite");
        let mut server = AzuriteServer {
            child,
            port,
            _data: data,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build the client"),
        };
        server.wait_ready().await?;
        Ok(server)
    }

    async fn wait_ready(&mut self) -> Result<(), String> {
        let url = format!("http://127.0.0.1:{}/", self.port);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("azurite exited during startup: {status}"));
            }
            if self.client.get(&url).send().await.is_ok() {
                return Ok(());
            }
            assert!(Instant::now() < deadline, "azurite did not become ready");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/{ACCOUNT_NAME}", self.port)
    }

    pub fn account_name(&self) -> &'static str {
        ACCOUNT_NAME
    }

    pub fn account_key(&self) -> &'static str {
        ACCOUNT_KEY
    }

    /// A PUT signed with the account key. `resource` is `container` or
    /// `container/blob`; `query` is only the canonicalized `name=value` pairs.
    async fn put_shared_key(
        &self,
        resource: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &'static [u8],
    ) -> reqwest::StatusCode {
        let date = rfc1123(now_unix());
        let mut ms_headers: Vec<(String, String)> = headers
            .iter()
            .filter(|(k, _)| k.starts_with("x-ms-"))
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ms_headers.push(("x-ms-date".into(), date.clone()));
        ms_headers.push(("x-ms-version".into(), API_VERSION.into()));
        ms_headers.sort();
        let canonical_headers: String = ms_headers
            .iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect();
        let mut sorted_query = query.to_vec();
        sorted_query.sort();
        let canonical_query: String = sorted_query
            .iter()
            .map(|(k, v)| format!("\n{k}:{v}"))
            .collect();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map_or("", |(_, v)| *v)
        };
        let length = if body.is_empty() {
            String::new()
        } else {
            body.len().to_string()
        };
        let to_sign = format!(
            "PUT\n\n\n{length}\n\n\n\n\n\n{}\n\n\n{canonical_headers}/{ACCOUNT_NAME}/{ACCOUNT_NAME}/{resource}{canonical_query}",
            header("if-none-match"),
        );
        let signature = hmac_base64(&account_key(), &to_sign);

        let query_string: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut url = format!("{}/{resource}", self.endpoint());
        if !query_string.is_empty() {
            url = format!("{url}?{}", query_string.join("&"));
        }
        let mut request = self
            .client
            .put(url)
            .header(
                "authorization",
                format!("SharedKey {ACCOUNT_NAME}:{signature}"),
            )
            .body(body);
        for (k, v) in &ms_headers {
            request = request.header(k, v);
        }
        if let Some(v) = headers.iter().find(|(k, _)| *k == "if-none-match") {
            request = request.header("if-none-match", v.1);
        }
        request.send().await.expect("send the request").status()
    }

    pub async fn create_container(&self, name: &str) {
        let status = self
            .put_shared_key(name, &[("restype", "container")], &[], b"")
            .await;
        assert!(status.is_success(), "create container {name}: {status}");
    }

    /// Put Blob signed with the account key; `if_none_match` sends
    /// `If-None-Match: *`.
    pub async fn put_blob_shared_key(
        &self,
        container: &str,
        blob: &str,
        body: &'static [u8],
        if_none_match: bool,
    ) -> reqwest::StatusCode {
        let mut headers = vec![("x-ms-blob-type", "BlockBlob")];
        if if_none_match {
            headers.push(("if-none-match", "*"));
        }
        self.put_shared_key(&format!("{container}/{blob}"), &[], &headers, body)
            .await
    }

    /// A request authorised by a container SAS, for attacks the store never
    /// sends. `query` is appended to the SAS, e.g. `comp=block&blockid=AAAA`.
    pub async fn sas_request(
        &self,
        method: &str,
        container: &str,
        blob: &str,
        sas: &str,
        query: &str,
        body: &'static [u8],
    ) -> reqwest::StatusCode {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("http method");
        let url = format!("{}/{container}/{blob}?{sas}&{query}", self.endpoint());
        self.client
            .request(method, url)
            .header("x-ms-version", API_VERSION)
            .header("x-ms-blob-type", "BlockBlob")
            .body(body)
            .send()
            .await
            .expect("send the request")
            .status()
    }

    /// A container SAS with the given permission letters, as a query string
    /// without the leading `?`.
    pub fn container_sas(&self, container: &str, permissions: &str, ttl: Duration) -> String {
        let expiry = iso8601(now_unix() + ttl.as_secs());
        let signature = percent_encode_signature(&container_sas_signature(
            ACCOUNT_NAME,
            ACCOUNT_KEY,
            container,
            permissions,
            None,
            &expiry,
            None,
            API_VERSION,
        ));
        format!("sv={API_VERSION}&sr=c&sp={permissions}&se={expiry}&sig={signature}")
    }
}

impl Drop for AzuriteServer {
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
        crate::assert_pinned("AZURITE_VERSION", AZURITE_VERSION);
        crate::assert_pinned("AZURITE_NPM_INTEGRITY", AZURITE_NPM_INTEGRITY);
        assert!(PACKAGE_JSON.contains(&format!("\"azurite\": \"{AZURITE_VERSION}\"")));
        assert!(PACKAGE_LOCK.contains(AZURITE_NPM_INTEGRITY));
    }

    #[test]
    fn dates_are_formatted_for_azure() {
        assert_eq!(rfc1123(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
