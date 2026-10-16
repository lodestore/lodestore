mod common;

use common::{TestServer, backends};
use reqwest::{Client, Response};
use serde_json::Value;

fn header(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
        .to_string()
}

fn references(html: &str) -> Vec<&str> {
    html.split(['"', '\''])
        .filter(|part| part.starts_with('/') || part.contains("://"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_reference_is_admin_only_and_self_contained() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (anonymous, base) = (Client::new(), ts.rest_url());

        let mut bundle_path = None;
        let api = ts.api().await;
        let page = api.get("/api/docs").await;
        assert_eq!(page.status(), 200);
        assert!(header(&page, "content-type").starts_with("text/html"));
        assert!(header(&page, "content-security-policy").starts_with("default-src 'self'"));
        let html = page.text().await.unwrap();
        assert!(!html.contains("http"), "{html}");
        for reference in references(&html) {
            assert!(reference.starts_with('/'), "{reference}");
            assert!(!reference.starts_with("//"), "{reference}");
            if reference.ends_with(".js") {
                bundle_path = Some(reference.to_string());
            }
        }
        let bundle_path = bundle_path.expect("the page loads a script");

        for path in ["/api/docs", bundle_path.as_str(), "/api/v1/openapi.json"] {
            let response = anonymous.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), 401, "{path}");
        }

        let bundle = api.get(&bundle_path).await;
        assert_eq!(bundle.status(), 200);
        assert_eq!(
            header(&bundle, "content-type"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            header(&bundle, "cache-control"),
            "private, max-age=31536000, immutable"
        );
        assert!(bundle.bytes().await.unwrap().len() > 1_000_000);

        let spec = api.get("/api/v1/openapi.json").await;
        assert_eq!(spec.status(), 200);
        assert_eq!(header(&spec, "content-type"), "application/json");
        let spec: Value = spec.json().await.unwrap();
        assert!(spec["openapi"].as_str().unwrap().starts_with("3.1"));
        assert!(spec["paths"]["/api/v1/server/info"].is_object());
        ts.stop().await;
    }
}
