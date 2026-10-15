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
            assert!(header(&page, "content-security-policy").starts_with("default-src 'self'"));
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
        assert!(index.contains("src=\"/theme-init.js\""), "{index}");
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

        let theme_init = client
            .get(format!("{base}/theme-init.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(theme_init.status(), 200);
        assert!(header(&theme_init, "content-type").starts_with("text/javascript"));
        assert_eq!(header(&theme_init, "cache-control"), "no-cache");
        assert!(header(&theme_init, "content-security-policy").starts_with("default-src 'self'"));

        let favicon = client
            .get(format!("{base}/favicon.svg"))
            .send()
            .await
            .unwrap();
        assert_eq!(favicon.status(), 200);
        assert_eq!(header(&favicon, "content-type"), "image/svg+xml");
        assert_eq!(header(&favicon, "cache-control"), "no-cache");

        let touch_icon = client
            .get(format!("{base}/apple-touch-icon.png"))
            .send()
            .await
            .unwrap();
        assert_eq!(touch_icon.status(), 200);
        assert_eq!(header(&touch_icon, "content-type"), "image/png");

        let main_script = asset.text().await.unwrap();
        let mut paths: Vec<String> = index
            .split('"')
            .filter(|part| part.starts_with("/assets/") && part.ends_with(".css"))
            .map(str::to_string)
            .collect();
        paths.extend(
            main_script
                .split(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
                .filter(|part| part.ends_with(".js"))
                .map(|name| format!("/assets/{name}")),
        );
        let mut bundles = vec![main_script];
        for path in paths {
            let chunk = client.get(format!("{base}{path}")).send().await.unwrap();
            if chunk.status() == 200 {
                bundles.push(chunk.text().await.unwrap());
            }
        }
        for (extension, content_type) in [("woff2", "font/woff2"), ("webp", "image/webp")] {
            let name = bundles
                .iter()
                .flat_map(|bundle| {
                    bundle.split(|c: char| !(c.is_ascii_alphanumeric() || "-_./".contains(c)))
                })
                .find(|part| part.ends_with(&format!(".{extension}")))
                .unwrap_or_else(|| panic!("no .{extension} referenced by the bundles"));
            let path = format!("/assets/{}", name.rsplit('/').next().unwrap());
            let file = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(file.status(), 200, "{path}");
            assert_eq!(header(&file, "content-type"), content_type, "{path}");
            assert_eq!(
                header(&file, "cache-control"),
                "public, max-age=31536000, immutable",
                "{path}"
            );
        }

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
