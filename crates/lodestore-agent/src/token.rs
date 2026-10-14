use data_encoding::BASE64URL_NOPAD;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::AgentError;

const PREFIX: &str = "lode1.";

#[derive(Deserialize)]
struct Payload {
    s: String,
    u: String,
    g: String,
    f: Option<String>,
}

pub struct JoinToken {
    pub secret: Zeroizing<String>,
    pub rest_url: String,
    pub grpc_address: String,
    pub ca_sha256: Option<String>,
}

impl std::fmt::Debug for JoinToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinToken")
            .field("rest_url", &self.rest_url)
            .field("grpc_address", &self.grpc_address)
            .finish_non_exhaustive()
    }
}

impl JoinToken {
    pub fn parse(text: &str) -> Result<JoinToken, AgentError> {
        let bad = |why: &str| AgentError::InvalidToken(why.to_string());
        let body = text
            .trim()
            .strip_prefix(PREFIX)
            .ok_or_else(|| bad("it does not start with lode1."))?;
        let raw = Zeroizing::new(
            BASE64URL_NOPAD
                .decode(body.as_bytes())
                .map_err(|_| bad("it is not valid base64url"))?,
        );
        let p: Payload = serde_json::from_slice(&raw).map_err(|_| bad("it is malformed"))?;
        if p.s.is_empty() || p.g.is_empty() {
            return Err(bad("it is incomplete"));
        }
        Ok(JoinToken {
            secret: Zeroizing::new(p.s),
            rest_url: p.u,
            grpc_address: p.g,
            ca_sha256: p.f.map(|f| f.to_ascii_lowercase()),
        })
    }
}
