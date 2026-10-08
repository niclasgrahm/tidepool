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
async fn request(method: &str, path: &str, body: Option<String>) -> Result<(u16, String), String> {
    let window = web_sys::window().ok_or("no window")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_comes_from_the_query_string() {
        assert_eq!(token_of("?token=abc"), Some("abc".into()));
        assert_eq!(token_of("?x=1&token=abc"), Some("abc".into()));
        assert_eq!(token_of("?token="), None);
        assert_eq!(token_of(""), None);
    }
}
