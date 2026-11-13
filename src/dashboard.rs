use crate::{
    config::StartupError,
    runtime::{HttpAdmission, RouteClass, Runtime},
};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path, sync::Arc};

pub const SCREENS: &[&str] = &[
    "/ui/",
    "/ui/keys",
    "/ui/admin/principals",
    "/ui/admin/principals/detail",
    "/ui/admin/usage",
    "/ui/admin/requests",
    "/ui/admin/credentials",
    "/ui/admin/models",
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    name: String,
    file: String,
    #[serde(default)]
    src: Option<String>,
    #[serde(default, rename = "isEntry")]
    is_entry: bool,
    #[serde(default)]
    css: Vec<String>,
}
struct Asset {
    bytes: Vec<u8>,
    mime: &'static str,
}
pub struct UiBundle {
    html: Vec<u8>,
    assets: BTreeMap<String, Asset>,
    csp: HeaderValue,
}
impl UiBundle {
    pub fn load(directory: &Path, sso_origin: &str) -> Result<Self, StartupError> {
        let invalid = || StartupError::UiUnavailable;
        let read = |name: &str, max| -> Result<Vec<u8>, StartupError> {
            let path = directory.join(name);
            let meta = std::fs::symlink_metadata(&path).map_err(|_| invalid())?;
            if !meta.is_file() || meta.len() > max {
                return Err(invalid());
            }
            std::fs::read(path).map_err(|_| invalid())
        };
        let manifest: BTreeMap<String, ManifestEntry> =
            serde_json::from_slice(&read(".vite/manifest.json", 64 * 1024)?)
                .map_err(|_| invalid())?;
        if manifest.len() != 1 {
            return Err(invalid());
        }
        let entry = manifest.get("index.html").ok_or_else(invalid)?;
        if entry.name != "index"
            || !entry.is_entry
            || entry.src.as_deref() != Some("index.html")
            || entry.css.len() != 1
        {
            return Err(invalid());
        }
        let mut assets = BTreeMap::new();
        for (name, mime, suffix) in [
            (&entry.file, "text/javascript; charset=utf-8", ".js"),
            (&entry.css[0], "text/css; charset=utf-8", ".css"),
        ] {
            if !valid_asset(name, suffix) {
                return Err(invalid());
            }
            assets.insert(
                format!("/ui/{name}"),
                Asset {
                    bytes: read(name, 2 * 1024 * 1024)?,
                    mime,
                },
            );
        }
        let html = read("index.html", 64 * 1024)?;
        let text = std::str::from_utf8(&html).map_err(|_| invalid())?;
        if !text.contains(&format!("src=\"/ui/{}\"", entry.file))
            || !text.contains(&format!("href=\"/ui/{}\"", entry.css[0]))
            || text.contains("<script>")
            || text.contains("<style>")
        {
            return Err(invalid());
        }
        let csp = HeaderValue::from_str(&format!("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self' {sso_origin}; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")).map_err(|_| invalid())?;
        Ok(Self { html, assets, csp })
    }
    pub fn routes(self: Arc<Self>, runtime: Arc<Runtime>) -> Router {
        let mut router = Router::new();
        for path in SCREENS {
            router = router.route(path, any(serve));
        }
        router
            .route("/ui/assets/{*path}", any(serve))
            .with_state(StaticState {
                bundle: self,
                runtime,
            })
    }
}
fn valid_asset(name: &str, suffix: &str) -> bool {
    name.len() < 200
        && name
            .strip_prefix("assets/index-")
            .and_then(|name| name.strip_suffix(suffix))
            .is_some_and(|hash| {
                hash.len() >= 8
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            })
}
#[derive(Clone)]
struct StaticState {
    bundle: Arc<UiBundle>,
    runtime: Arc<Runtime>,
}
async fn serve(State(state): State<StaticState>, request: Request) -> Response {
    if request.method() != Method::GET {
        return crate::http::method_not_allowed();
    }
    let path = request.uri().path();
    // raw URI만 비교하므로 percent decoding에 의한 경로 승격은 없다.
    let html = SCREENS.contains(&path);
    let asset = state.bundle.assets.get(path);
    if !html && asset.is_none() {
        return crate::http::route_error(
            StatusCode::NOT_FOUND,
            "route_not_found",
            "등록되지 않은 경로입니다.",
        );
    }
    let Some(http) = request.extensions().get::<HttpAdmission>() else {
        return crate::http::admission_error(crate::runtime::AdmissionError::InvalidTransition);
    };
    let lease = match state
        .runtime
        .try_admit_with_http(RouteClass::Asset, http.clone())
    {
        Ok(lease) => lease,
        Err(error) => return crate::http::admission_error(error),
    };
    let (bytes, mime, cache) = if html {
        (
            state.bundle.html.clone(),
            "text/html; charset=utf-8",
            "no-cache",
        )
    } else {
        let a = asset.expect("검사한 asset");
        (
            a.bytes.clone(),
            a.mime,
            "public, max-age=31536000, immutable",
        )
    };
    let mut response = Body::from(bytes).into_response();
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert("content-security-policy", state.bundle.csp.clone());
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    lease.bind_response(response)
}
pub async fn redirect(method: Method) -> Response {
    if method != Method::GET {
        return crate::http::method_not_allowed();
    }
    let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/ui/"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
