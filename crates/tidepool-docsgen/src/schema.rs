//! JSON Schema (as schemars writes it) to markdown: a table of fields for an
//! object, a table of values for an enum, and a section for every named type
//! it refers to.
//!
//! Only the shapes schemars produces from tidepool's types are handled:
//! `$ref` into `$defs`, `anyOf` with `null` for an optional value, `oneOf` of
//! `const`s for a documented enum, `enum` for an undocumented one, arrays and
//! maps. Anything else reads as `any`, which a reviewer of the site notices.

use std::collections::{BTreeSet, VecDeque};

use serde_json::{Map, Value};

const DEFS: &str = "#/$defs/";

/// Renders schemas into one partial. Named types are rendered once each,
/// after everything that refers to them, as `##` sections.
pub struct Renderer<'a> {
    defs: Map<String, Value>,
    /// Anchor prefix, so types of the same name on two pages (or in two
    /// partials of one page) don't collide.
    prefix: &'a str,
    /// Named types already rendered, or queued, or rendered elsewhere.
    seen: BTreeSet<String>,
    queue: VecDeque<String>,
    /// Names that are rendered elsewhere under the same prefix (the API's
    /// top-level schemas): linked, never queued.
    external: BTreeSet<String>,
}

impl<'a> Renderer<'a> {
    #[must_use]
    pub fn new(prefix: &'a str) -> Self {
        Self {
            defs: Map::new(),
            prefix,
            seen: BTreeSet::new(),
            queue: VecDeque::new(),
            external: BTreeSet::new(),
        }
    }

    /// Names that are given sections of their own by the caller.
    pub fn external(&mut self, names: impl IntoIterator<Item = String>) {
        for name in names {
            self.seen.insert(name.clone());
            self.external.insert(name);
        }
    }

    /// The anchor of a named type.
    #[must_use]
    pub fn anchor(&self, name: &str) -> String {
        anchor(self.prefix, name)
    }

    /// A schema's body: its description, then its fields or values. Its
    /// `$defs` join the ones this renderer knows.
    pub fn body(&mut self, schema: &Value) -> String {
        if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
            for (name, def) in defs {
                self.defs.entry(name.clone()).or_insert_with(|| def.clone());
            }
        }
        let mut out = String::new();
        if let Some(text) = schema.get("description").and_then(Value::as_str) {
            out.push_str(&paragraphs(text));
            out.push_str("\n\n");
        }
        out.push_str(&self.shape(schema));
        out
    }

    /// Every named type referred to so far, and the ones they refer to, each
    /// as a `##` section.
    pub fn sections(&mut self) -> String {
        let mut out = String::new();
        while let Some(name) = self.queue.pop_front() {
            let Some(def) = self.defs.get(&name).cloned() else {
                continue;
            };
            out.push_str(&format!(
                "## `{name}` {{#{}}}\n\n",
                anchor(self.prefix, &name)
            ));
            out.push_str(&self.body(&def));
            out.push_str("\n\n");
        }
        out
    }

    /// What a schema is, at length: a table of fields, a table of values, or
    /// a list of alternatives.
    fn shape(&mut self, schema: &Value) -> String {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            return self.fields(schema, properties);
        }
        if let Some(values) = documented_values(schema) {
            let mut out = String::from("| value | description |\n| --- | --- |\n");
            for (value, description) in values {
                out.push_str(&format!("| `{value}` | {} |\n", cell(&description)));
            }
            return out;
        }
        let alternatives = schema
            .get("oneOf")
            .or_else(|| schema.get("anyOf"))
            .and_then(Value::as_array);
        if let Some(alternatives) = alternatives {
            let mut out = String::from("One of:\n\n");
            for alternative in alternatives {
                if is_null(alternative) {
                    continue;
                }
                out.push_str(&format!("- {}", self.type_of(alternative)));
                if let Some(text) = alternative.get("description").and_then(Value::as_str) {
                    out.push_str(&format!(": {}", inline(text)));
                }
                out.push('\n');
            }
            return out;
        }
        format!("Type: {}.\n", self.type_of(schema))
    }

    fn fields(&mut self, schema: &Value, properties: &Map<String, Value>) -> String {
        let required: BTreeSet<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut out =
            String::from("| key | type | default | description |\n| --- | --- | --- | --- |\n");
        for (key, field) in properties {
            let kind = self.type_of(field);
            let default = if required.contains(key.as_str()) {
                "**required**".to_string()
            } else {
                // an object's defaults are its own fields' defaults, shown
                // in its section
                field
                    .get("default")
                    .filter(|d| !d.is_null() && !d.is_object())
                    .map_or_else(String::new, |d| format!("`{}`", literal(d)))
            };
            let description = field
                .get("description")
                .and_then(Value::as_str)
                .map_or_else(String::new, cell);
            out.push_str(&format!(
                "| `{key}` | {kind} | {default} | {description} |\n"
            ));
        }
        out
    }

    /// What a schema is, in a few words, for a table cell. Queues the named
    /// types it refers to.
    pub fn type_of(&mut self, schema: &Value) -> String {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let name = reference.strip_prefix(DEFS).unwrap_or(reference);
            return self.link(name);
        }
        if let Some(value) = schema.get("const") {
            return format!("`{}`", literal(value));
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values
                .iter()
                .map(|v| format!("`{}`", literal(v)))
                .collect::<Vec<_>>()
                .join(" · ");
        }
        if let Some(values) = documented_values(schema) {
            return values
                .iter()
                .map(|(v, _)| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(" · ");
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(alternatives) = schema.get(key).and_then(Value::as_array) {
                let kinds: Vec<String> = alternatives
                    .iter()
                    .filter(|a| !is_null(a))
                    .map(|a| self.type_of(a))
                    .collect();
                return kinds.join(" or ");
            }
        }
        match schema.get("type") {
            Some(Value::Array(types)) => {
                let named: Vec<&Value> = types
                    .iter()
                    .filter(|t| t.as_str() != Some("null"))
                    .collect();
                match named.as_slice() {
                    [one] => {
                        let mut single = schema.clone();
                        if let Some(map) = single.as_object_mut() {
                            map.insert("type".into(), (*one).clone());
                        }
                        self.type_of(&single)
                    }
                    _ => named
                        .iter()
                        .filter_map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(" or "),
                }
            }
            Some(Value::String(kind)) => match kind.as_str() {
                "array" => match schema.get("items") {
                    Some(items) => format!("list of {}", self.type_of(items)),
                    None => "list".to_string(),
                },
                "object" => self.object(schema),
                other => other.to_string(),
            },
            _ => "any".to_string(),
        }
    }

    fn object(&mut self, schema: &Value) -> String {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            let keys: Vec<String> = properties.keys().map(|k| format!("`{k}`")).collect();
            return format!("object with {}", keys.join(", "));
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) | None => "object".to_string(),
            Some(Value::Bool(true)) => "map".to_string(),
            Some(values) => format!("map of {}", self.type_of(values)),
        }
    }

    fn link(&mut self, name: &str) -> String {
        if !self.seen.contains(name) {
            self.seen.insert(name.to_string());
            self.queue.push_back(name.to_string());
        }
        if self.external.contains(name) || self.defs.contains_key(name) {
            format!("[`{name}`](#{})", anchor(self.prefix, name))
        } else {
            format!("`{name}`")
        }
    }
}

/// An enum whose values each carry a description: schemars' `oneOf` of
/// `const`s.
fn documented_values(schema: &Value) -> Option<Vec<(String, String)>> {
    let alternatives = schema.get("oneOf")?.as_array()?;
    alternatives
        .iter()
        .map(|a| {
            let value = a.get("const")?;
            let description = a
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some((literal(value), description.to_string()))
        })
        .collect()
}

fn is_null(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
}

/// A value as it is written in YAML: strings bare, the rest as JSON.
fn literal(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The anchor of a named type under a prefix: lowercase, as `VitePress`' own
/// anchors are.
#[must_use]
pub fn anchor(prefix: &str, name: &str) -> String {
    format!("{prefix}-{}", name.to_lowercase())
}

/// Doc-comment text as markdown paragraphs: rustdoc's hard wraps joined,
/// blank lines kept.
#[must_use]
pub fn paragraphs(text: &str) -> String {
    text.split("\n\n")
        .map(|p| escape(&p.split('\n').map(str::trim).collect::<Vec<_>>().join(" ")))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Doc-comment text for one line of a list.
fn inline(text: &str) -> String {
    paragraphs(text).replace("\n\n", " ")
}

/// Doc-comment text for a table cell: one line, pipes escaped.
#[must_use]
pub fn cell(text: &str) -> String {
    paragraphs(text)
        .replace("\n\n", "<br><br>")
        .replace('|', "\\|")
}

/// Outside code spans, what Vue would read as markup: a bare `<` starts an
/// element and `{{` an interpolation, and either fails the site's build.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, part) in text.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 {
            out.push_str(part);
        } else {
            out.push_str(
                &part
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace("{{", "&#123;&#123;"),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_optional_reference_links_and_queues_its_type() {
        let mut r = Renderer::new("t");
        let schema = json!({
            "type": "object",
            "properties": {
                "at": { "anyOf": [{ "$ref": "#/$defs/Grid" }, { "type": "null" }], "description": "Where." }
            },
            "$defs": { "Grid": { "type": "object", "properties": { "x": { "type": "integer" } }, "required": ["x"] } }
        });
        let body = r.body(&schema);
        assert!(
            body.contains("| `at` | [`Grid`](#t-grid) |  | Where. |"),
            "{body}"
        );
        let sections = r.sections();
        assert!(sections.contains("## `Grid` {#t-grid}"), "{sections}");
        assert!(
            sections.contains("| `x` | integer | **required** |"),
            "{sections}"
        );
    }

    #[test]
    fn documented_values_become_a_table() {
        let mut r = Renderer::new("t");
        let schema = json!({ "oneOf": [
            { "const": "list", "description": "A list." },
            { "const": "tree", "description": "A tree." }
        ]});
        assert_eq!(r.type_of(&schema), "`list` · `tree`");
        let body = r.body(&schema);
        assert!(body.contains("| `tree` | A tree. |"), "{body}");
    }

    #[test]
    fn markup_outside_code_is_escaped() {
        assert_eq!(
            escape("a <b> `<c>` {{d}}"),
            "a &lt;b&gt; `<c>` &#123;&#123;d}}"
        );
        assert_eq!(
            cell("one\ntwo\n\nthree | four"),
            "one two<br><br>three \\| four"
        );
    }
}
