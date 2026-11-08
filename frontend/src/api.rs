//! Talking to the server's JSON API, with the request and response types from
//! `tidepool-protocol`: the UI and the server share one definition of every body.
//!
//! A `?token=` the page was opened with is sent as the bearer token, the
//! same one the live socket uses.

use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

/// The `token` of a query string (`?a=1&token=x`), if any.
#[must_use]
pub fn token_of(search: &str) -> Option<String> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// One request to the server this UI came from: the status and the body.
/// The server's address when the page wasn't served by it (an MCP App):
/// its `<meta name="tidepool-origin">`.
#[must_use]
pub fn origin() -> Option<String> {
    meta("tidepool-origin").map(|o| o.trim_end_matches('/').to_string())
}

/// A `<meta name=…>`'s content.
#[must_use]
pub fn meta(name: &str) -> Option<String> {
    web_sys::window()?
        .document()?
        .query_selector(&format!("meta[name=\"{name}\"]"))
        .ok()??
        .get_attribute("content")
}

async fn request(method: &str, path: &str, body: Option<String>) -> Result<(u16, String), String> {
    let window = web_sys::window().ok_or("no window")?;
    let path = &format!("{}{path}", origin().unwrap_or_default());
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    let headers = web_sys::Headers::new().map_err(|err| format!("{err:?}"))?;
    if let Some(token) = token_of(&window.location().search().unwrap_or_default()) {
        let _ = headers.set("authorization", &format!("Bearer {token}"));
    }
    if let Some(body) = body {
        let _ = headers.set("content-type", "application/json");
        init.set_body(&JsValue::from_str(&body));
    }
    init.set_headers(&headers);
    let response = JsFuture::from(window.fetch_with_str_and_init(path, &init))
        .await
        .map_err(|err| format!("{err:?}"))?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| "fetch did not return a Response")?;
    let text = JsFuture::from(response.text().map_err(|err| format!("{err:?}"))?)
        .await
        .map_err(|err| format!("{err:?}"))?;
    Ok((
        response.status(),
        text.as_string().ok_or("response body was not text")?,
    ))
}

/// `GET` a path and decode the JSON body.
///
/// Errors are strings because all the UI does with them is show them.
pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let (status, text) = request("GET", path, None).await?;
    if !(200..300).contains(&status) {
        return Err(format!("{path} answered {status}"));
    }
    serde_json::from_str(&text).map_err(|err| format!("decoding {path}: {err}"))
}

/// `POST` a JSON body; the status and the decoded reply (any status: error
/// bodies carry what the UI shows).
pub async fn post_json<B: Serialize>(
    path: &str,
    body: &B,
) -> Result<(u16, serde_json::Value), String> {
    let body = serde_json::to_string(body).map_err(|err| err.to_string())?;
    let (status, text) = request("POST", path, Some(body)).await?;
    let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    Ok((status, value))
}

/// Trade `token` for a session cookie (`POST /v1/auth/session`).
pub async fn start_session(token: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    let headers = web_sys::Headers::new().map_err(|err| format!("{err:?}"))?;
    let _ = headers.set("authorization", &format!("Bearer {token}"));
    init.set_headers(&headers);
    let response = JsFuture::from(window.fetch_with_str_and_init("/v1/auth/session", &init))
        .await
        .map_err(|err| format!("{err:?}"))?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| "fetch did not return a Response")?;
    if response.ok() {
        Ok(())
    } else {
        Err(format!("the token wasn't accepted ({})", response.status()))
    }
}

/// The page's query string without `token=`.
#[must_use]
pub fn without_token(search: &str) -> String {
    let kept: Vec<&str> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("token="))
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

/// If the page was opened with `?token=`, trade it for a session and take
/// it out of the address bar (and so out of history and shared links).
pub async fn adopt_token() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let search = location.search().unwrap_or_default();
    let Some(token) = token_of(&search) else {
        return;
    };
    if start_session(&token).await.is_ok()
        && let Ok(history) = window.history()
    {
        let url = format!(
            "{}{}{}",
            location.pathname().unwrap_or_default(),
            without_token(&search),
            location.hash().unwrap_or_default()
        );
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&url));
    }
}

/// Save `text` as a file named `name` in the browser's downloads.
pub fn save_file(name: &str, text: &str, media_type: &str) -> Result<(), String> {
    let parts = js_sys::Array::of1(&JsValue::from_str(text));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(media_type);
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)
        .map_err(|err| format!("{err:?}"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|err| format!("{err:?}"))?;
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let link: web_sys::HtmlAnchorElement = document
        .create_element("a")
        .map_err(|err| format!("{err:?}"))?
        .dyn_into()
        .map_err(|_| "not an anchor")?;
    link.set_href(&url);
    link.set_download(name);
    link.click();
    let _ = web_sys::Url::revoke_object_url(&url);
    Ok(())
}

/// `POST` a JSON body; the status and the body as text.
pub async fn post_text<B: Serialize>(path: &str, body: &B) -> Result<(u16, String), String> {
    let body = serde_json::to_string(body).map_err(|err| err.to_string())?;
    request("POST", path, Some(body)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_leaves_the_query_string_and_the_rest_stays() {
        assert_eq!(without_token("?token=abc"), "");
        assert_eq!(without_token("?x=1&token=abc&y=2"), "?x=1&y=2");
        assert_eq!(without_token(""), "");
    }

    #[test]
    fn the_token_comes_from_the_query_string() {
        assert_eq!(token_of("?token=abc"), Some("abc".into()));
        assert_eq!(token_of("?x=1&token=abc"), Some("abc".into()));
        assert_eq!(token_of("?token="), None);
        assert_eq!(token_of(""), None);
    }
}
