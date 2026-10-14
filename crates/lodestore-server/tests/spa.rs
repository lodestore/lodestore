#![cfg(feature = "embed-ui")]

mod common;

use common::{TestServer, backends};
use reqwest::Client;
use serde_json::Value;

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spa_serves_index_and_assets() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());

        for path in ["/", "/hosts", "/setup"] {
            let page = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(page.status(), 200, "{path}");
            assert!(header(&page, "content-type").starts_with("text/html"));
            assert_eq!(header(&page, "cache-control"), "no-cache");
            let html = page.text().await.unwrap();
            assert!(html.contains("<div id=\"root\">"), "{html}");
        }

        let index = client
            .get(&base)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let script = index
            .split('"')
            .find(|part| part.starts_with("/assets/") && part.ends_with(".js"))
            .expect("index.html references a script");
        let asset = client.get(format!("{base}{script}")).send().await.unwrap();
        assert_eq!(asset.status(), 200);
        assert!(header(&asset, "content-type").starts_with("text/javascript"));
        assert_eq!(
            header(&asset, "cache-control"),
            "public, max-age=31536000, immutable"
        );

        let missing_asset = client
            .get(format!("{base}/assets/nope.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_asset.status(), 404);

        let api = client
            .get(format!("{base}/api/v1/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(api.status(), 404);
        let body: Value = api.json().await.unwrap();
        assert_eq!(body["code"], "not_found");
        ts.stop().await;
    }
}
