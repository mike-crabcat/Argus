//! Browser entry point and small web helpers.

mod alerts;
mod app;
mod phones;
mod plan;
mod replay;
mod search;
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
    request("GET", path, None, token).await
}

/// An API request with an optional JSON body; returns the body text.
pub(crate) async fn request(
    method: &str,
    path: &str,
    json: Option<&str>,
    token: Option<&str>,
) -> Result<String, String> {
    let window = web_sys::window().unwrap();
    let init = RequestInit::new();
    init.set_method(method);
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    if let Some(t) = token {
        headers
            .set("Authorization", &format!("Bearer {t}"))
            .map_err(|e| format!("{e:?}"))?;
    }
    if let Some(body) = json {
        headers
            .set("Content-Type", "application/json")
            .map_err(|e| format!("{e:?}"))?;
        init.set_body(&wasm_bindgen::JsValue::from_str(body));
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
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .unwrap_or_default();
    if !resp.ok() {
        let detail = text.trim();
        return Err(if detail.is_empty() || detail.len() > 200 {
            format!("HTTP {}", resp.status())
        } else {
            detail.to_owned()
        });
    }
    Ok(text)
}

/// A request with any body; returns (status, body text). Only network
/// failures are errors.
pub(crate) async fn send(
    method: &str,
    path: &str,
    body: Option<&wasm_bindgen::JsValue>,
    content_type: Option<&str>,
    token: Option<&str>,
) -> Result<(u16, String), String> {
    let window = web_sys::window().unwrap();
    let init = RequestInit::new();
    init.set_method(method);
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    if let Some(t) = token {
        headers
            .set("Authorization", &format!("Bearer {t}"))
            .map_err(|e| format!("{e:?}"))?;
    }
    if let Some(ct) = content_type {
        headers
            .set("Content-Type", ct)
            .map_err(|e| format!("{e:?}"))?;
    }
    if let Some(b) = body {
        init.set_body(b);
    }
    init.set_headers(&headers);
    let resp: Response = JsFuture::from(window.fetch_with_str_and_init(path, &init))
        .await
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not a Response".to_string())?;
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .unwrap_or_default();
    Ok((resp.status(), text))
}

/// Downloads an image with the token and decodes it in the browser to
/// RGBA, scaled down to at most `max` pixels a side. Returns the decoded
/// size, the original size and the pixels.
pub(crate) async fn fetch_image(
    path: &str,
    token: Option<&str>,
    max: u32,
) -> Result<([usize; 2], [u32; 2], Vec<u8>), String> {
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
    if !resp.ok() {
        return Err(format!("image: HTTP {}", resp.status()));
    }
    let blob: web_sys::Blob = JsFuture::from(resp.blob().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not a Blob".to_string())?;
    let bmp: web_sys::ImageBitmap = JsFuture::from(
        window
            .create_image_bitmap_with_blob(&blob)
            .map_err(|e| format!("{e:?}"))?,
    )
    .await
    .map_err(|e| format!("{e:?}"))?
    .dyn_into()
    .map_err(|_| "not an ImageBitmap".to_string())?;
    let (ow, oh) = (bmp.width(), bmp.height());
    let k = (max as f64 / ow.max(oh) as f64).min(1.0);
    let (w, h) = (
        ((ow as f64 * k).round() as u32).max(1),
        ((oh as f64 * k).round() as u32).max(1),
    );
    let canvas: web_sys::HtmlCanvasElement = window
        .document()
        .unwrap()
        .create_element("canvas")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not a canvas".to_string())?;
    canvas.set_width(w);
    canvas.set_height(h);
    // A CPU-backed canvas: reading back a GPU one can give blank pixels.
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"willReadFrequently".into(), &true.into());
    let cx: web_sys::CanvasRenderingContext2d = canvas
        .get_context_with_context_options("2d", &opts)
        .map_err(|e| format!("{e:?}"))?
        .ok_or("no 2d context")?
        .dyn_into()
        .map_err(|_| "not a 2d context".to_string())?;
    cx.draw_image_with_image_bitmap_and_dw_and_dh(&bmp, 0.0, 0.0, w as f64, h as f64)
        .map_err(|e| format!("{e:?}"))?;
    let data = cx
        .get_image_data(0, 0, w as i32, h as i32)
        .map_err(|e| format!("{e:?}"))?;
    bmp.close();
    Ok(([w as usize, h as usize], [ow, oh], data.data().0))
}

/// Opens the browser's file picker for an image; `on_file` gets the file.
/// The input stays in the page (id `argus-file`) so scripts can fill it.
pub(crate) fn pick_image(on_file: impl Fn(web_sys::File) + 'static) {
    let document = web_sys::window().unwrap().document().unwrap();
    let input: web_sys::HtmlInputElement = match document.get_element_by_id("argus-file") {
        Some(el) => el.dyn_into().unwrap(),
        None => {
            let el: web_sys::HtmlInputElement = document
                .create_element("input")
                .unwrap()
                .dyn_into()
                .unwrap();
            el.set_type("file");
            el.set_id("argus-file");
            el.set_accept("image/png,image/jpeg");
            let _ = el.style().set_property("display", "none");
            let _ = document.body().unwrap().append_child(&el);
            el
        }
    };
    let target = input.clone();
    let cb = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        if let Some(f) = target.files().and_then(|l| l.get(0)) {
            on_file(f);
        }
        target.set_value("");
    });
    input.set_onchange(Some(cb.as_ref().unchecked_ref()));
    cb.forget();
    input.click();
}

/// The page's origin, e.g. `http://127.0.0.1:8080`.
pub(crate) fn origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}
