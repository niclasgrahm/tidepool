//! Running inside an AI app, as an MCP App (`ui://tidepool/view`). The
//! page's own script speaks the bridge to the host (`window.tidepoolBridge`,
//! see `tidepool_server::apps`); this is what the UI makes of it, with no
//! DOM: which view to show and how to reach it, from the tool result, and
//! what to tell the host's model.

use serde_json::{Value, json};
use tidepool_protocol::live::SelectedField;

/// What a tool result tells the app: the view, where the server is, and
/// the token that opens the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub view: String,
    pub origin: String,
    pub token: Option<String>,
}

/// The target of a `tools/call` result (its `_meta.tidepool`).
#[must_use]
pub fn target(result: &Value) -> Option<Target> {
    let meta = &result["_meta"]["tidepool"];
    Some(Target {
        view: meta["view"].as_str()?.to_string(),
        origin: meta["origin"].as_str()?.trim_end_matches('/').to_string(),
        token: meta["token"].as_str().map(str::to_string),
    })
}

/// The live socket's address for a target.
#[must_use]
pub fn socket_url(target: &Target) -> String {
    let base = target
        .origin
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            target
                .origin
                .strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| target.origin.clone());
    match &target.token {
        Some(token) => format!("{base}/v1/ws?token={token}"),
        None => format!("{base}/v1/ws"),
    }
}

/// What the host's model is told after the person changes the selection
/// (`ui/update-model-context`): the selection in words, and as data.
#[must_use]
pub fn model_context(view: &str, session: &str, selection: &[SelectedField], epoch: u64) -> Value {
    let said: Vec<String> = selection.iter().map(describe).collect();
    let text = if said.is_empty() {
        format!("In Tidepool view {view}, nothing is selected.")
    } else {
        format!(
            "In Tidepool view {view}, the person selected {}.",
            said.join("; ")
        )
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": {
            "view": view,
            "session": session,
            "epoch": epoch,
            "selection": selection,
        },
    })
}

/// One selected field, in words.
fn describe(field: &SelectedField) -> String {
    let word = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
    if let Some(values) = &field.values {
        let values: Vec<String> = values.iter().map(word).collect();
        return format!("{} = {}", field.field, values.join(", "));
    }
    if let Some(text) = &field.contains {
        return format!("{} contains \"{text}\"", field.field);
    }
    match (&field.from, &field.to) {
        (Some(from), Some(to)) => format!("{} from {} to {}", field.field, word(from), word(to)),
        (Some(from), None) => format!("{} from {}", field.field, word(from)),
        (None, Some(to)) => format!("{} before {}", field.field, word(to)),
        (None, None) => field.field.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_names_the_view_and_how_to_reach_it() {
        let result = json!({
            "structuredContent": { "view": "~ab" },
            "_meta": { "tidepool": { "view": "~ab", "origin": "https://t.example.com/", "token": "embed_x" } },
        });
        let Some(target) = target(&result) else {
            panic!("a target in {result}")
        };
        assert_eq!(target.view, "~ab");
        assert_eq!(
            socket_url(&target),
            "wss://t.example.com/v1/ws?token=embed_x"
        );
        let local = Target {
            view: "~ab".into(),
            origin: "http://127.0.0.1:7070".into(),
            token: None,
        };
        assert_eq!(socket_url(&local), "ws://127.0.0.1:7070/v1/ws");
        assert!(super::target(&json!({"content": []})).is_none());
    }

    #[test]
    fn the_model_is_told_the_selection_in_words_and_as_data() {
        let selected = |field: &str, values: Vec<Value>| SelectedField {
            field: field.into(),
            values: Some(values),
            from: None,
            to: None,
            relative: None,
            contains: None,
        };
        let context = model_context(
            "~ab",
            "s1",
            &[selected(
                "customers.region",
                vec![json!("north"), json!("east")],
            )],
            7,
        );
        assert_eq!(
            context["content"][0]["text"],
            "In Tidepool view ~ab, the person selected customers.region = north, east."
        );
        assert_eq!(
            context["structuredContent"]["selection"][0]["values"],
            json!(["north", "east"])
        );
        assert_eq!(context["structuredContent"]["epoch"], 7);
        let none = model_context("~ab", "s1", &[], 7);
        assert_eq!(
            none["content"][0]["text"],
            "In Tidepool view ~ab, nothing is selected."
        );
    }
}
