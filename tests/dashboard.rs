use axum::{body::Body, http::Request};
use m_gated::{
    config::Limits,
    http::{self, AppState},
    runtime::Runtime,
};
use tower::ServiceExt;

#[tokio::test]
async fn static_routes_assets_and_csp_are_explicit() {
    let router = http::router(AppState {
        runtime: Runtime::new(Limits::default()).unwrap(),
    });
    let response = router
        .oneshot(
            Request::builder()
                .uri("/ui?return_to=https://evil.invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        308,
        "고정 /ui redirect는 query를 반사하지 않는다"
    );
    assert_eq!(response.headers()["location"], "/ui/");
}

struct BundleFixture(std::path::PathBuf);
impl BundleFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mgated-u0-static-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.join(".vite")).unwrap();
        std::fs::create_dir(path.join("assets")).unwrap();
        std::fs::write(path.join(".vite/manifest.json"),r#"{"index.html":{"file":"assets/index-12345678.js","name":"index","src":"index.html","isEntry":true,"css":["assets/index-12345678.css"]}}"#).unwrap();
        std::fs::write(path.join("index.html"),r#"<!doctype html><script type="module" src="/ui/assets/index-12345678.js"></script><link rel="stylesheet" href="/ui/assets/index-12345678.css">"#).unwrap();
        std::fs::write(
            path.join("assets/index-12345678.js"),
            "document.title='m-gated';",
        )
        .unwrap();
        std::fs::write(path.join("assets/index-12345678.css"), "body{color:black}").unwrap();
        Self(path)
    }
}
impl Drop for BundleFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
#[tokio::test]
async fn fixed_vite_manifest_is_loaded_without_accepting_unknown_manifest_fields() {
    let fixture = BundleFixture::new();
    let bundle = m_gated::dashboard::UiBundle::load(&fixture.0, "https://auth.example.test");
    assert!(
        bundle.is_ok(),
        "고정 Vite7.3.1의 name 필드가 있는 실제 형식을 수용한다"
    );
    let runtime = Runtime::new(Limits::default()).unwrap();
    runtime.start_http().unwrap();
    runtime.record_health(
        m_gated::runtime::Dependency::Ui,
        true,
        tokio::time::Instant::now(),
    );
    let router = http::router_with_components(
        AppState {
            runtime: runtime.clone(),
        },
        None,
        std::sync::Arc::new(bundle.unwrap()).routes(runtime.clone()),
    );
    for path in m_gated::dashboard::SCREENS {
        let r = router
            .clone()
            .oneshot(Request::builder().uri(*path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
        assert_eq!(r.headers()["cache-control"], "no-cache");
        let csp = r.headers()["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("unsafe-"));
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    }
    for (path, mime) in [
        (
            "/ui/assets/index-12345678.js",
            "text/javascript; charset=utf-8",
        ),
        ("/ui/assets/index-12345678.css", "text/css; charset=utf-8"),
    ] {
        let r = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], mime);
        assert_eq!(
            r.headers()["cache-control"],
            "public, max-age=31536000, immutable"
        );
    }
    for path in [
        "/ui/missing",
        "/ui/assets/missing.js",
        "/ui/assets/%2e%2e/index.html",
        "/ui/assets/%2findex-12345678.js",
        "/ui/assets/../index.html",
        "/ui/admin/missing",
        "/gateway/missing",
        "/auth/login/missing",
    ] {
        let r = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
    for method in ["HEAD", "OPTIONS", "POST"] {
        let r = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/ui/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 405);
        assert_eq!(r.headers()["allow"], "GET");
    }
    let manifest = std::fs::read_to_string(fixture.0.join(".vite/manifest.json")).unwrap();
    std::fs::write(
        fixture.0.join(".vite/manifest.json"),
        manifest.replace("\"name\":\"index\"", "\"unknown\":\"index\""),
    )
    .unwrap();
    assert!(m_gated::dashboard::UiBundle::load(&fixture.0, "https://auth.example.test").is_err());
    runtime.stop();
}

#[test]
fn vite_url_safe_hash_preserves_internal_hyphen_and_underscore() {
    for hash in ["7y-FXxkE", "_12-45_8", "12345678"] {
        let fixture = BundleFixture::new();
        for name in [".vite/manifest.json", "index.html"] {
            let original = std::fs::read_to_string(fixture.0.join(name)).unwrap();
            std::fs::write(fixture.0.join(name), original.replace("12345678", hash)).unwrap();
        }
        for suffix in ["js", "css"] {
            std::fs::rename(
                fixture.0.join(format!("assets/index-12345678.{suffix}")),
                fixture.0.join(format!("assets/index-{hash}.{suffix}")),
            )
            .unwrap();
        }
        assert!(
            m_gated::dashboard::UiBundle::load(&fixture.0, "https://auth.example.test").is_ok(),
            "합법 URL-safe hash {hash}"
        );
    }
}

#[test]
fn vite_asset_name_requires_fixed_entry_and_bounded_url_safe_hash() {
    for hash in ["short", "1234567.", "1234/678", "1234..78", "1234é678"] {
        let fixture = BundleFixture::new();
        let original = std::fs::read_to_string(fixture.0.join(".vite/manifest.json")).unwrap();
        std::fs::write(
            fixture.0.join(".vite/manifest.json"),
            original.replace("12345678", hash),
        )
        .unwrap();
        assert!(
            m_gated::dashboard::UiBundle::load(&fixture.0, "https://auth.example.test").is_err()
        );
    }
}
