//! Talking to the server's JSON API, with the request and response types from
//! `tidepool-protocol`: the UI and the server share one definition of every body.

use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

/// `GET` a path on the server this UI was served from, and decode the JSON body.
///
/// Errors are strings because all the UI does with them is show them.
pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = JsFuture::from(window.fetch_with_str(path))
        .await
        .map_err(|err| format!("{err:?}"))?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| "fetch did not return a Response")?;
    if !response.ok() {
        return Err(format!("{path} answered {}", response.status()));
    }
    let text = JsFuture::from(response.text().map_err(|err| format!("{err:?}"))?)
        .await
        .map_err(|err| format!("{err:?}"))?;
    let text = text.as_string().ok_or("response body was not text")?;
    serde_json::from_str(&text).map_err(|err| format!("decoding {path}: {err}"))
}
