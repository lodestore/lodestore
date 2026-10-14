//! An HTTP proxy in front of an S3 server that fails requests on purpose and
//! counts what passes through. The client signs requests for the proxy's
//! address, so the `Host` header is forwarded untouched.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::Response;

const SLOW_DOWN: &str =
    "<Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message></Error>";
const ACCESS_DENIED: &str =
    "<Error><Code>AccessDenied</Code><Message>Access Denied.</Message></Error>";
const NOT_IMPLEMENTED: &str =
    "<Error><Code>NotImplemented</Code><Message>Not implemented.</Message></Error>";
const INTERNAL_ERROR: &str =
    "<Error><Code>InternalError</Code><Message>We encountered an internal error.</Message></Error>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAction {
    /// Answer 503 SlowDown without contacting the upstream.
    SlowDown,
    /// Let the upstream process the request, then answer 500 instead of its
    /// response: the write happened but the client cannot know.
    ForwardThenFail,
    /// Answer 403 AccessDenied to a request without `If-None-Match`, as a
    /// provider that enforces the `s3:if-none-match` policy condition does.
    DenyUnconditional,
    /// Answer 501 NotImplemented without contacting the upstream: a failure
    /// that is permanent but says nothing about permissions.
    NotImplemented,
}

#[derive(Debug, Clone)]
pub struct FaultRule {
    pub method: &'static str,
    /// Matched against the object key, i.e. the path after `/<bucket>/`.
    pub key_prefix: String,
    pub action: FaultAction,
    /// The rule fires on the first `times` matching requests.
    pub times: u32,
}

#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    pub rules: Vec<FaultRule>,
}

impl FaultPlan {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn with(mut self, rule: FaultRule) -> Self {
        self.rules.push(rule);
        self
    }
}

/// Request counts keyed by method and the first segment of the object key.
#[derive(Debug, Default)]
pub struct Counters {
    accepted: Mutex<BTreeMap<(String, String), u64>>,
    forwarded: Mutex<BTreeMap<(String, String), u64>>,
    multipart: AtomicU64,
}

impl Counters {
    fn bump(map: &Mutex<BTreeMap<(String, String), u64>>, method: &str, segment: &str) {
        *map.lock()
            .unwrap()
            .entry((method.to_owned(), segment.to_owned()))
            .or_default() += 1;
    }

    fn get(map: &Mutex<BTreeMap<(String, String), u64>>, method: &str, segment: &str) -> u64 {
        map.lock()
            .unwrap()
            .get(&(method.to_owned(), segment.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    /// Requests that reached the proxy, including the ones it failed itself.
    pub fn accepted(&self, method: &str, segment: &str) -> u64 {
        Self::get(&self.accepted, method, segment)
    }

    /// Requests the proxy passed on to the upstream.
    pub fn forwarded(&self, method: &str, segment: &str) -> u64 {
        Self::get(&self.forwarded, method, segment)
    }

    /// Requests that belong to a multipart upload.
    pub fn multipart(&self) -> u64 {
        self.multipart.load(Ordering::Relaxed)
    }
}

struct Inner {
    upstream: String,
    client: reqwest::Client,
    rules: Mutex<Vec<(FaultRule, u32)>>,
    counters: Arc<Counters>,
}

pub struct FaultProxy {
    addr: SocketAddr,
    counters: Arc<Counters>,
    task: tokio::task::JoinHandle<()>,
}

impl FaultProxy {
    /// `upstream` is the base URL of the S3 server, e.g. `http://127.0.0.1:9000`.
    pub async fn start(upstream: &str, plan: FaultPlan) -> FaultProxy {
        let counters = Arc::new(Counters::default());
        let inner = Arc::new(Inner {
            upstream: upstream.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build the upstream client"),
            rules: Mutex::new(
                plan.rules
                    .into_iter()
                    .map(|r| (r.clone(), r.times))
                    .collect(),
            ),
            counters: Arc::clone(&counters),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let addr = listener.local_addr().expect("proxy address");
        let app = Router::new().fallback(handle).with_state(inner);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FaultProxy {
            addr,
            counters,
            task,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.counters)
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn xml(status: StatusCode, body: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(body))
        .unwrap()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "transfer-encoding" | "te" | "trailer" | "upgrade"
    )
}

async fn handle(State(st): State<Arc<Inner>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return xml(StatusCode::BAD_REQUEST, INTERNAL_ERROR);
    };

    let path = parts.uri.path().trim_start_matches('/');
    let key = path.split_once('/').map_or("", |(_, k)| k);
    let segment = key.split('/').next().unwrap_or("");
    let verb = parts.method.as_str();
    let query = parts.uri.query().unwrap_or("");

    Counters::bump(&st.counters.accepted, verb, segment);
    if query.split('&').any(|p| {
        let name = p.split('=').next().unwrap_or("");
        name == "uploads" || name == "uploadId"
    }) {
        st.counters.multipart.fetch_add(1, Ordering::Relaxed);
    }

    let action = {
        let mut rules = st.rules.lock().unwrap();
        rules
            .iter_mut()
            .find(|(r, left)| {
                *left > 0
                    && r.method == verb
                    && key.starts_with(&r.key_prefix)
                    && (r.action != FaultAction::DenyUnconditional
                        || !parts.headers.contains_key(header::IF_NONE_MATCH))
            })
            .map(|(r, left)| {
                *left -= 1;
                r.action
            })
    };
    if action == Some(FaultAction::SlowDown) {
        return xml(StatusCode::SERVICE_UNAVAILABLE, SLOW_DOWN);
    }

    if action == Some(FaultAction::NotImplemented) {
        return xml(StatusCode::NOT_IMPLEMENTED, NOT_IMPLEMENTED);
    }
    if action == Some(FaultAction::DenyUnconditional) {
        return xml(StatusCode::FORBIDDEN, ACCESS_DENIED);
    }

    let url = format!(
        "{}{}",
        st.upstream,
        parts.uri.path_and_query().map_or("/", |pq| pq.as_str())
    );
    let mut forward = st.client.request(parts.method.clone(), url).body(body);
    for (name, value) in &parts.headers {
        // Host stays as the client signed it; length is recomputed from the body.
        if !is_hop_by_hop(name) && name != header::CONTENT_LENGTH {
            forward = forward.header(name, value);
        }
    }
    Counters::bump(&st.counters.forwarded, verb, segment);
    let upstream = match forward.send().await {
        Ok(r) => r,
        Err(_) => return xml(StatusCode::BAD_GATEWAY, INTERNAL_ERROR),
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let Ok(bytes) = upstream.bytes().await else {
        return xml(StatusCode::BAD_GATEWAY, INTERNAL_ERROR);
    };
    if action == Some(FaultAction::ForwardThenFail) {
        return xml(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR);
    }

    let mut resp = Response::builder().status(status);
    for (name, value) in &headers {
        if !is_hop_by_hop(name) {
            resp = resp.header(name, value.clone());
        }
    }
    resp.body(Body::from(bytes)).unwrap()
}
