use std::path::PathBuf;

#[test]
fn openapi_json_is_current() {
    let spec = lodestore_server::api::openapi();
    let generated = format!("{}\n", spec.to_pretty_json().unwrap());
    assert!(generated.contains("\"openapi\": \"3.1"));

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/openapi.json");
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "web/openapi.json is stale; regenerate with UPDATE_OPENAPI=1 cargo nextest run -p lodestore-server openapi_json_is_current"
    );
}
