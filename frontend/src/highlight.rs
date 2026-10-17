//! Syntax highlighting for the config editor. Every config file is YAML
//! (`tidepool.yaml`, `model.ossie.yaml`, `tables/*.yaml`,
//! `dashboards/*.yaml`), so this is one YAML tokenizer: keys, strings,
//! numbers, `true`/`false`/`null`, comments, anchors and tags, and the
//! punctuation between them.
//!
//! It works a line at a time and never fails: text that isn't valid YAML is
//! still split into tokens that cover it exactly, because the editor is
//! showing a draft as it's typed. Only block scalars (`|`, `>`) carry state
//! from one line to the next.

/// What a piece of text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Key,
    Str,
    Number,
    Literal,
    Comment,
    Punct,
    Meta,
}

impl Kind {
    /// The CSS class the editor colours it with (`hl-…`), if any.
    fn class(self) -> Option<&'static str> {
        match self {
            Kind::Plain => None,
            Kind::Key => Some("hl-key"),
            Kind::Str => Some("hl-str"),
            Kind::Number => Some("hl-num"),
            Kind::Literal => Some("hl-lit"),
            Kind::Comment => Some("hl-comment"),
            Kind::Punct => Some("hl-punct"),
            Kind::Meta => Some("hl-meta"),
        }
    }
}

/// `text` as tokens, in order: joined, they are `text` again.
#[must_use]
pub fn tokens(text: &str) -> Vec<(Kind, &str)> {
    let mut out = Vec::new();
    // the indentation a block scalar's lines must exceed, while in one
    let mut block: Option<usize> = None;
    for whole in text.split_inclusive('\n') {
        let line = whole.trim_end_matches(['\n', '\r']);
        let end = &whole[line.len()..];
        let indent = line.len() - line.trim_start_matches(' ').len();
        if let Some(parent) = block {
            if line.trim().is_empty() {
                push(&mut out, Kind::Plain, line);
                push(&mut out, Kind::Plain, end);
                continue;
            }
            if indent > parent {
                push(&mut out, Kind::Plain, &line[..indent]);
                push(&mut out, Kind::Str, &line[indent..]);
                push(&mut out, Kind::Plain, end);
                continue;
            }
        }
        block = tokenize_line(line, indent, &mut out);
        push(&mut out, Kind::Plain, end);
    }
    out
}

/// `text` highlighted, as HTML for a `<pre>`: escaped, with each token
/// that has a colour in a `<span class="hl-…">`.
#[must_use]
pub fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for (kind, piece) in tokens(text) {
        match kind.class() {
            Some(class) => {
                out.push_str("<span class=\"");
                out.push_str(class);
                out.push_str("\">");
                escape_into(&mut out, piece);
                out.push_str("</span>");
            }
            None => escape_into(&mut out, piece),
        }
    }
    out
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn push<'a>(out: &mut Vec<(Kind, &'a str)>, kind: Kind, text: &'a str) {
    if !text.is_empty() {
        out.push((kind, text));
    }
}

fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

fn is_flow(b: u8) -> bool {
    matches!(b, b',' | b'[' | b']' | b'{' | b'}')
}

/// One line (without its line break) outside a block scalar. Returns the
/// indentation a block scalar starting on this line needs its lines to
/// exceed, when one does.
fn tokenize_line<'a>(
    line: &'a str,
    indent: usize,
    out: &mut Vec<(Kind, &'a str)>,
) -> Option<usize> {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    // flow collections open on this line ({ … }, [ … ])
    let mut depth = 0usize;
    // the column of the node a block scalar here would belong to
    let mut parent = indent;
    let mut block = None;

    if (line.starts_with("---") || line.starts_with("...")) && (n == 3 || is_ws(bytes[3])) {
        push(out, Kind::Meta, &line[..3]);
        i = 3;
    }
    while i < n {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        let ends = |at: Option<u8>| at.is_none_or(is_ws);
        if is_ws(c) {
            let start = i;
            while i < n && is_ws(bytes[i]) {
                i += 1;
            }
            push(out, Kind::Plain, &line[start..i]);
            continue;
        }
        if c == b'#' && (i == 0 || is_ws(bytes[i - 1])) {
            push(out, Kind::Comment, &line[i..]);
            break;
        }
        if depth == 0 && (c == b'-' || c == b'?') && ends(next) {
            parent = i;
            push(out, Kind::Punct, &line[i..=i]);
            i += 1;
            continue;
        }
        if c == b':' && (ends(next) || (depth > 0 && next.is_some_and(is_flow))) {
            push(out, Kind::Punct, &line[i..=i]);
            i += 1;
            continue;
        }
        if matches!(c, b'{' | b'[' | b'}' | b']') || (c == b',' && depth > 0) {
            match c {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
            push(out, Kind::Punct, &line[i..=i]);
            i += 1;
            continue;
        }
        if depth == 0
            && (c == b'|' || c == b'>')
            && let Some(len) = block_header(&line[i..])
        {
            push(out, Kind::Meta, &line[i..i + len]);
            block = Some(parent);
            i += len;
            continue;
        }
        if c == b'&' || c == b'*' || c == b'!' {
            let start = i;
            i += 1;
            while i < n && !is_ws(bytes[i]) && !(depth > 0 && is_flow(bytes[i])) {
                i += 1;
            }
            push(out, Kind::Meta, &line[start..i]);
            continue;
        }

        // a scalar: quoted or plain, and a key when a `:` follows it
        let start = i;
        let quoted = c == b'"' || c == b'\'';
        let mut end = if c == b'"' {
            double_quoted(bytes, i)
        } else if c == b'\'' {
            single_quoted(bytes, i)
        } else {
            plain(bytes, i, depth)
        };
        if end <= start {
            end = start + line[start..].chars().next().map_or(1, char::len_utf8);
        }
        let text = &line[start..end];
        let mut m = end;
        while m < n && is_ws(bytes[m]) {
            m += 1;
        }
        let after = bytes.get(m + 1).copied();
        let key = m < n
            && bytes[m] == b':'
            && (ends(after) || (depth > 0 && (quoted || after.is_some_and(is_flow))));
        if key {
            // the colon too: `"a":1` is a key in a flow collection
            parent = start;
            push(out, Kind::Key, text);
            push(out, Kind::Plain, &line[end..m]);
            push(out, Kind::Punct, &line[m..=m]);
            i = m + 1;
            continue;
        }
        push(out, if quoted { Kind::Str } else { classify(text) }, text);
        i = end;
    }
    block
}

/// The end of a `"…"` scalar starting at `i` (the line's end when it
/// isn't closed on it).
fn double_quoted(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// The end of a `'…'` scalar starting at `i`, where `''` is a quote.
fn single_quoted(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\'' {
            if b.get(j + 1) == Some(&b'\'') {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// The end of a plain scalar starting at `i`, trailing blanks left out.
fn plain(bytes: &[u8], i: usize, depth: usize) -> usize {
    let n = bytes.len();
    let mut j = i;
    while j < n {
        let c = bytes[j];
        if c == b'#' && j > i && is_ws(bytes[j - 1]) {
            break;
        }
        if c == b':' {
            let after = bytes.get(j + 1).copied();
            if after.is_none_or(is_ws) || (depth > 0 && after.is_some_and(is_flow)) {
                break;
            }
        }
        if depth > 0 && is_flow(c) {
            break;
        }
        j += 1;
    }
    while j > i && is_ws(bytes[j - 1]) {
        j -= 1;
    }
    j
}

/// The length of a block scalar's header (`|`, `>-`, `|+2`) when `rest`
/// starts with one and nothing but a comment follows it.
fn block_header(rest: &str) -> Option<usize> {
    let len = 1 + rest[1..]
        .bytes()
        .take_while(|c| matches!(c, b'+' | b'-' | b'0'..=b'9'))
        .count();
    let tail = rest[len..].trim_start_matches([' ', '\t']);
    let spaced = tail.len() < rest.len() - len;
    (tail.is_empty() || (spaced && tail.starts_with('#'))).then_some(len)
}

/// A plain scalar that isn't a key: a number, `true`/`false`/`null`, or a
/// string.
fn classify(text: &str) -> Kind {
    const LITERALS: [&str; 10] = [
        "true", "True", "TRUE", "false", "False", "FALSE", "null", "Null", "NULL", "~",
    ];
    if LITERALS.contains(&text) {
        Kind::Literal
    } else if is_number(text) {
        Kind::Number
    } else {
        Kind::Str
    }
}

/// YAML 1.2's numbers: decimal, `0x`/`0o`, and `.inf`/`.nan`.
fn is_number(text: &str) -> bool {
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    if matches!(unsigned, ".inf" | ".Inf" | ".INF") || matches!(text, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    if let Some(hex) = text.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|c| c.is_ascii_hexdigit());
    }
    if let Some(oct) = text.strip_prefix("0o") {
        return !oct.is_empty() && oct.bytes().all(|c| matches!(c, b'0'..=b'7'));
    }
    let starts = unsigned.as_bytes().first().is_some_and(u8::is_ascii_digit)
        || (unsigned.starts_with('.')
            && unsigned.as_bytes().get(1).is_some_and(u8::is_ascii_digit));
    starts
        && unsigned
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        && unsigned.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The non-plain tokens of `text`, for comparing.
    fn marked(text: &str) -> Vec<(Kind, &str)> {
        tokens(text)
            .into_iter()
            .filter(|(k, t)| *k != Kind::Plain || !t.trim().is_empty())
            .collect()
    }

    #[test]
    fn tokens_cover_the_text_exactly() {
        let samples = [
            "",
            "\n\n",
            "a: b\r\nc: d",
            "key: 'it''s' # note\n",
            "x: \"unclosed\\\n",
            "- - a\n  - b: [1, {c: d}]\n",
            "]]}},,: ::x #a",
            "naïve: é ü # ß\nü: |\n  ö\n",
            "&a *b !c |x >-\n",
            "-\n?\n:\n",
            "\"\\é",
        ];
        for sample in samples {
            let joined: String = tokens(sample).iter().map(|(_, t)| *t).collect();
            assert_eq!(joined, sample, "tokens of {sample:?} don't add up to it");
        }
    }

    #[test]
    fn every_example_file_is_covered_exactly() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/retail");
        for path in [
            "tidepool.yaml",
            "model.ossie.yaml",
            "tables/orders.yaml",
            "dashboards/sales.yaml",
        ] {
            let text = match std::fs::read_to_string(format!("{root}/{path}")) {
                Ok(text) => text,
                Err(err) => panic!("expected to read {path}: {err}"),
            };
            let joined: String = tokens(&text).iter().map(|(_, t)| *t).collect();
            assert_eq!(joined, text, "tokens of {path} don't add up to it");
            assert!(
                tokens(&text).iter().any(|(k, _)| *k == Kind::Key),
                "{path} should have keys"
            );
        }
    }

    #[test]
    fn keys_values_and_comments_are_told_apart() {
        assert_eq!(
            marked("name: orders   # the table\n"),
            vec![
                (Kind::Key, "name"),
                (Kind::Punct, ":"),
                (Kind::Str, "orders"),
                (Kind::Comment, "# the table"),
            ]
        );
        assert_eq!(
            marked("  - n: 12\n    ok: true\n    none: ~\n    at: 1.5e3\n"),
            vec![
                (Kind::Punct, "-"),
                (Kind::Key, "n"),
                (Kind::Punct, ":"),
                (Kind::Number, "12"),
                (Kind::Key, "ok"),
                (Kind::Punct, ":"),
                (Kind::Literal, "true"),
                (Kind::Key, "none"),
                (Kind::Punct, ":"),
                (Kind::Literal, "~"),
                (Kind::Key, "at"),
                (Kind::Punct, ":"),
                (Kind::Number, "1.5e3"),
            ]
        );
    }

    #[test]
    fn plain_scalars_keep_colons_and_hashes_that_dont_end_them() {
        assert_eq!(
            marked("url: http://x/#top\n"),
            vec![
                (Kind::Key, "url"),
                (Kind::Punct, ":"),
                (Kind::Str, "http://x/#top")
            ]
        );
        assert_eq!(
            marked("epoch_interval: 200ms\n"),
            vec![
                (Kind::Key, "epoch_interval"),
                (Kind::Punct, ":"),
                (Kind::Str, "200ms")
            ]
        );
    }

    #[test]
    fn flow_collections_split_on_their_punctuation() {
        assert_eq!(
            marked("expression: { dialects: [{ dialect: ANSI_SQL, expression: \"a: b\" }] }"),
            vec![
                (Kind::Key, "expression"),
                (Kind::Punct, ":"),
                (Kind::Punct, "{"),
                (Kind::Key, "dialects"),
                (Kind::Punct, ":"),
                (Kind::Punct, "["),
                (Kind::Punct, "{"),
                (Kind::Key, "dialect"),
                (Kind::Punct, ":"),
                (Kind::Str, "ANSI_SQL"),
                (Kind::Punct, ","),
                (Kind::Key, "expression"),
                (Kind::Punct, ":"),
                (Kind::Str, "\"a: b\""),
                (Kind::Punct, "}"),
                (Kind::Punct, "]"),
                (Kind::Punct, "}"),
            ]
        );
        // JSON-style keys may touch their colon inside a flow collection
        assert_eq!(
            marked("{\"a\":1}"),
            vec![
                (Kind::Punct, "{"),
                (Kind::Key, "\"a\""),
                (Kind::Punct, ":"),
                (Kind::Number, "1"),
                (Kind::Punct, "}"),
            ]
        );
    }

    #[test]
    fn quoted_strings_end_at_their_own_quote() {
        assert_eq!(
            marked("data: '{\"x\": \"it''s\"}' # json\n"),
            vec![
                (Kind::Key, "data"),
                (Kind::Punct, ":"),
                (Kind::Str, "'{\"x\": \"it''s\"}'"),
                (Kind::Comment, "# json"),
            ]
        );
        assert_eq!(
            marked("a: \"say \\\"hi\\\" # not a comment\""),
            vec![
                (Kind::Key, "a"),
                (Kind::Punct, ":"),
                (Kind::Str, "\"say \\\"hi\\\" # not a comment\""),
            ]
        );
    }

    #[test]
    fn block_scalars_run_until_the_indentation_comes_back() {
        let text = "  - note: |-\n      first: not a key\n\n      # not a comment\n    next: 1\n";
        assert_eq!(
            marked(text),
            vec![
                (Kind::Punct, "-"),
                (Kind::Key, "note"),
                (Kind::Punct, ":"),
                (Kind::Meta, "|-"),
                (Kind::Str, "first: not a key"),
                (Kind::Str, "# not a comment"),
                (Kind::Key, "next"),
                (Kind::Punct, ":"),
                (Kind::Number, "1"),
            ]
        );
    }

    #[test]
    fn documents_anchors_and_tags_are_meta() {
        assert_eq!(
            marked("---\nbase: &b { x: 1 }\nother: *b\nt: !!str 3\n"),
            vec![
                (Kind::Meta, "---"),
                (Kind::Key, "base"),
                (Kind::Punct, ":"),
                (Kind::Meta, "&b"),
                (Kind::Punct, "{"),
                (Kind::Key, "x"),
                (Kind::Punct, ":"),
                (Kind::Number, "1"),
                (Kind::Punct, "}"),
                (Kind::Key, "other"),
                (Kind::Punct, ":"),
                (Kind::Meta, "*b"),
                (Kind::Key, "t"),
                (Kind::Punct, ":"),
                (Kind::Meta, "!!str"),
                (Kind::Number, "3"),
            ]
        );
    }

    #[test]
    fn numbers_are_yaml_numbers_only() {
        for yes in [
            "0", "-3", "+4.5", ".5", "1e9", "0x1F", "0o17", ".inf", "-.inf", ".nan",
        ] {
            assert_eq!(classify(yes), Kind::Number, "{yes} should be a number");
        }
        for no in ["2024-01-01", "1.2.3", "75%", "e5", "0x", "inf", "nan", "-"] {
            assert_eq!(classify(no), Kind::Str, "{no} should be a string");
        }
    }

    #[test]
    fn html_is_escaped_and_classed() {
        assert_eq!(
            html("a: <b> & c # x\n"),
            "<span class=\"hl-key\">a</span><span class=\"hl-punct\">:</span> \
             <span class=\"hl-str\">&lt;b&gt; &amp; c</span> \
             <span class=\"hl-comment\"># x</span>\n"
        );
    }
}
