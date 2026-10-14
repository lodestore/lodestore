use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, Endpoint};
use tower::Service;

use crate::{AgentError, NetOptions};

pub fn host_of(address: &str) -> &str {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match address.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => address,
    }
}

fn tunnel_for(net: &NetOptions, target: &http::Uri) -> Option<Tunnel<HttpConnector>> {
    let https = net.https_proxy.as_ref().or(net.http_proxy.as_ref());
    let matcher = Matcher::builder()
        .https(https.cloned().unwrap_or_default())
        .http(net.http_proxy.clone().unwrap_or_default())
        .no(net.no_proxy.clone().unwrap_or_default())
        .build();
    let intercept = matcher.intercept(target)?;
    let tunnel = Tunnel::new(intercept.uri().clone(), HttpConnector::new());
    Some(match intercept.basic_auth() {
        Some(auth) => tunnel.with_auth(auth.clone()),
        None => tunnel,
    })
}

pub async fn channel(
    grpc_address: &str,
    server_name: &str,
    tls: Arc<ClientConfig>,
    net: &NetOptions,
) -> Result<Channel, AgentError> {
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|e| AgentError::Connect(format!("invalid server name {server_name}: {e}")))?;
    let target: http::Uri = format!("https://{grpc_address}")
        .parse()
        .map_err(|e| AgentError::Connect(format!("invalid server address {grpc_address}: {e}")))?;
    let tunnel = tunnel_for(net, &target);
    let connector = TlsConnector::from(tls);
    let address = grpc_address.to_string();
    let service = tower::service_fn(move |_uri: http::Uri| {
        let connector = connector.clone();
        let name = name.clone();
        let address = address.clone();
        let target = target.clone();
        let tunnel = tunnel.clone();
        async move {
            let tcp = match tunnel {
                Some(mut tunnel) => {
                    std::future::poll_fn(|cx| tunnel.poll_ready(cx))
                        .await
                        .map_err(std::io::Error::other)?;
                    tunnel
                        .call(target)
                        .await
                        .map_err(std::io::Error::other)?
                        .into_inner()
                }
                None => TcpStream::connect(address.as_str()).await?,
            };
            tcp.set_nodelay(true)?;
            let stream = connector.connect(name, tcp).await?;
            Ok::<_, std::io::Error>(TokioIo::new(stream))
        }
    });
    Endpoint::from_shared(format!("http://{grpc_address}"))
        .map_err(|e| AgentError::Connect(e.to_string()))?
        .connect_timeout(Duration::from_secs(15))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_while_idle(true)
        .connect_with_connector(service)
        .await
        .map_err(|e| AgentError::Connect(error_chain(&e)))
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}
