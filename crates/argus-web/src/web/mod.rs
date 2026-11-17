//! Browser entry point and small web helpers.

mod app;
mod replay;
mod theme;
mod tile;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{RequestInit, Response};

#[wasm_bindgen(start)]
pub fn start() {
    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window().unwrap().document().unwrap();
        let canvas = document
            .get_element_by_id("argus")
            .expect("canvas #argus")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
            )
            .await;
        if let Err(e) = result {
            set_status(&format!("Argus failed to start: {e:?}"));
        }
    });
}

/// Writes into the page's `#status` element (shown until the app runs, and
/// on fatal errors).
fn set_status(msg: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("status"))
    {
        el.set_text_content(Some(msg));
    }
}

/// Client wall-clock time in microseconds, with sub-millisecond resolution.
pub(crate) fn now_us() -> i64 {
    let p = web_sys::window().unwrap().performance().unwrap();
    ((p.time_origin() + p.now()) * 1000.0) as i64
}

/// Milliseconds on the page's monotonic clock.
pub(crate) fn now_ms() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

/// What [`get_text`] returns when the server wants a (different) token.
pub(crate) const UNAUTHORIZED: &str = "unauthorized";

const TOKEN_KEY: &str = "argus.token";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// The token remembered in this browser, if any.
pub(crate) fn stored_token() -> Option<String> {
    local_storage()?
        .get_item(TOKEN_KEY)
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
}

/// Remembers (or with `None`, forgets) the token in this browser. Storage
/// may be unavailable (private windows); that only means asking again.
pub(crate) fn store_token(token: Option<&str>) {
    if let Some(s) = local_storage() {
        let _ = match token {
            Some(t) => s.set_item(TOKEN_KEY, t),
            None => s.remove_item(TOKEN_KEY),
        };
    }
}

/// Takes `?token=` out of the address bar (and so out of history and
/// anything copied from it), keeping the rest of the URL.
pub(crate) fn strip_token_from_url() {
    let Some(w) = web_sys::window() else { return };
    let loc = w.location();
    let (Ok(path), Ok(search), Ok(hash)) = (loc.pathname(), loc.search(), loc.hash()) else {
        return;
    };
    let rest = crate::without_param(&search, "token");
    if rest != search {
        if let Ok(h) = w.history() {
            let _ = h.replace_state_with_url(
                &wasm_bindgen::JsValue::NULL,
                "",
                Some(&format!("{path}{rest}{hash}")),
            );
        }
    }
}

/// `?token=` from the page URL, forwarded to the API.
pub(crate) fn page_token() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search)
        .ok()?
        .get("token")
        .filter(|t| !t.is_empty())
}

/// The WebSocket URL for a path on this server (ws: or wss: to match).
pub(crate) fn ws_url(path: &str, token: Option<&str>) -> String {
    let loc = web_sys::window().unwrap().location();
    let scheme = if loc.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = loc.host().unwrap_or_default();
    match token {
        Some(t) => format!("{scheme}://{host}{path}?token={}", urlencode(t)),
        None => format!("{scheme}://{host}{path}"),
    }
}

fn urlencode(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// GET a JSON API path with the bearer token; returns the body text.
pub(crate) async fn get_text(path: &str, token: Option<&str>) -> Result<String, String> {
    let window = web_sys::window().unwrap();
    let init = RequestInit::new();
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    if let Some(t) = token {
        headers
            .set("Authorization", &format!("Bearer {t}"))
            .map_err(|e| format!("{e:?}"))?;
    }
    init.set_headers(&headers);
    let resp: Response = JsFuture::from(window.fetch_with_str_and_init(path, &init))
        .await
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not a Response".to_string())?;
    if resp.status() == 401 {
        return Err(UNAUTHORIZED.into());
    }
    if !resp.ok() {
        return Err(format!("{path}: HTTP {}", resp.status()));
    }
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?;
    text.as_string().ok_or_else(|| "empty response".into())
}
