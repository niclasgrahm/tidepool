//! Config files as text, and where things are in them.
//!
//! The types in this crate are deserialized with `serde_norway`, which knows
//! where a *type* error happened but not where any particular value sits.
//! Errors found later, across files ("relationship `x` points to unknown
//! dataset `y`"), need that, so every file also gets a [`PathIndex`]: one pass
//! over the YAML events that records the location of every value (and every
//! mapping key) under its path, e.g. `relationships[1].to`. Validation code
//! then reports problems by path and the renderer looks the path up.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use saphyr_parser::{Event, Parser, ScalarStyle, Span};

/// A position in a file: 1-based line and column, and the length of the token
/// in characters (at least 1), for underlining.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub line: usize,
    pub col: usize,
    pub len: usize,
}

/// A path to a value inside a YAML document, written the way people read
/// it: `datasets[0].fields[2].name`. The empty path is the document root.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct YamlPath(Vec<Segment>);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Segment {
    Key(String),
    Index(usize),
}

impl YamlPath {
    #[must_use]
    pub fn root() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn key(&self, key: impl Into<String>) -> Self {
        let mut segments = self.0.clone();
        segments.push(Segment::Key(key.into()));
        Self(segments)
    }

    #[must_use]
    pub fn index(&self, index: usize) -> Self {
        let mut segments = self.0.clone();
        segments.push(Segment::Index(index));
        Self(segments)
    }

    /// The path one level up, or `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let mut segments = self.0.clone();
        segments.pop()?;
        Some(Self(segments))
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for YamlPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            match segment {
                Segment::Key(key) if i == 0 => write!(f, "{key}")?,
                Segment::Key(key) => write!(f, ".{key}")?,
                Segment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// Where every value and mapping key of one YAML document is.
#[derive(Clone, Debug, Default)]
pub struct PathIndex {
    values: HashMap<YamlPath, Location>,
    keys: HashMap<YamlPath, Location>,
    top_level_keys: Vec<String>,
}

impl PathIndex {
    /// Index the first document in `text`. A file that doesn't parse as YAML
    /// gets whatever was indexed before the error; serde reports the error
    /// itself, with its own location.
    #[must_use]
    pub fn build(text: &str) -> Self {
        let mut index = Self::default();
        let mut stack: Vec<Frame> = Vec::new();
        let mut root_done = false;

        for event in Parser::new_from_str(text) {
            let Ok((event, span)) = event else { break };
            match event {
                Event::Scalar(value, style, ..) => {
                    let quotes =
                        if matches!(style, ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted) {
                            2
                        } else {
                            0
                        };
                    if let Some(Frame::Map { path, key: None }) = stack.last_mut() {
                        // a mapping key
                        let key_path = path.key(value.as_ref());
                        if path.is_root() {
                            index.top_level_keys.push(value.to_string());
                        }
                        index
                            .keys
                            .insert(key_path, location(span, value.chars().count() + quotes));
                        if let Some(Frame::Map { key, .. }) = stack.last_mut() {
                            *key = Some(value.to_string());
                        }
                    } else {
                        let path = child_path(&stack);
                        index
                            .values
                            .insert(path, location(span, value.chars().count() + quotes));
                        advance(&mut stack);
                    }
                }
                Event::Alias(_) => {
                    let path = child_path(&stack);
                    index.values.insert(path, location(span, 1));
                    advance(&mut stack);
                }
                Event::MappingStart(..) | Event::SequenceStart(..) => {
                    let path = child_path(&stack);
                    index.values.insert(path.clone(), location(span, 1));
                    stack.push(if matches!(event, Event::MappingStart(..)) {
                        Frame::Map { path, key: None }
                    } else {
                        Frame::Seq { path, next: 0 }
                    });
                }
                Event::MappingEnd | Event::SequenceEnd => {
                    stack.pop();
                    advance(&mut stack);
                }
                Event::DocumentEnd if root_done => break,
                Event::DocumentEnd => root_done = true,
                _ => {}
            }
        }
        index
    }

    /// The location of the value at `path`, or of its closest indexed
    /// ancestor (a value that is missing entirely is best pointed at through
    /// the object that should have had it).
    #[must_use]
    pub fn value(&self, path: &YamlPath) -> Option<Location> {
        let mut path = path.clone();
        loop {
            if let Some(location) = self.values.get(&path) {
                return Some(*location);
            }
            path = path.parent()?;
        }
    }

    /// The location of the mapping key whose value is at `path`, falling back
    /// to the value's location.
    #[must_use]
    pub fn key(&self, path: &YamlPath) -> Option<Location> {
        self.keys.get(path).copied().or_else(|| self.value(path))
    }

    /// The keys of the root mapping, in document order.
    #[must_use]
    pub fn top_level_keys(&self) -> &[String] {
        &self.top_level_keys
    }
}

enum Frame {
    /// Inside a mapping; `key` is the key whose value comes next, or `None`
    /// when the next scalar is itself a key.
    Map {
        path: YamlPath,
        key: Option<String>,
    },
    Seq {
        path: YamlPath,
        next: usize,
    },
}

/// The path of the value about to start, given where we are.
fn child_path(stack: &[Frame]) -> YamlPath {
    match stack.last() {
        None => YamlPath::root(),
        Some(Frame::Map { path, key }) => path.key(key.clone().unwrap_or_default()),
        Some(Frame::Seq { path, next }) => path.index(*next),
    }
}

/// A value just finished: move the enclosing collection on to its next slot.
fn advance(stack: &mut [Frame]) {
    match stack.last_mut() {
        Some(Frame::Map { key, .. }) => *key = None,
        Some(Frame::Seq { next, .. }) => *next += 1,
        None => {}
    }
}

fn location(span: Span, len: usize) -> Location {
    Location {
        line: span.start.line(),
        col: span.start.col() + 1,
        len: len.max(1),
    }
}

/// One config file: where it lives (relative to the config directory), its
/// text, and its path index.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub path: PathBuf,
    pub text: String,
    pub index: PathIndex,
}

impl SourceFile {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        let text = text.into();
        let index = PathIndex::build(&text);
        Self {
            path: path.into(),
            text,
            index,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The text of a 1-based line, without its newline.
    #[must_use]
    pub fn line(&self, line: usize) -> Option<&str> {
        self.text.lines().nth(line.checked_sub(1)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
name: retail
datasets:
  - name: orders
    source: orders
  - name: customers
    fields:
      - name: region
relationships: []
";

    fn path(segments: &[&str]) -> YamlPath {
        segments
            .iter()
            .fold(YamlPath::root(), |path, segment| match segment.parse() {
                Ok(index) => path.index(index),
                Err(_) => path.key(*segment),
            })
    }

    #[test]
    fn paths_print_the_way_people_read_them() {
        assert_eq!(
            path(&["datasets", "1", "fields", "0", "name"]).to_string(),
            "datasets[1].fields[0].name"
        );
        assert_eq!(YamlPath::root().to_string(), "");
    }

    #[test]
    fn values_are_found_by_path() {
        let index = PathIndex::build(DOC);
        let at = |segments: &[&str]| index.value(&path(segments)).map(|l| (l.line, l.col, l.len));
        assert_eq!(at(&["name"]), Some((1, 7, 6)));
        assert_eq!(at(&["datasets", "0", "source"]), Some((4, 13, 6)));
        assert_eq!(
            at(&["datasets", "1", "fields", "0", "name"]),
            Some((7, 15, 6))
        );
        assert_eq!(at(&["datasets", "1", "name"]), Some((5, 11, 9)));
    }

    #[test]
    fn keys_are_found_by_the_path_of_their_value() {
        let index = PathIndex::build(DOC);
        let key = index.key(&path(&["datasets", "0", "source"]));
        assert_eq!(key.map(|l| (l.line, l.col)), Some((4, 5)));
    }

    #[test]
    fn a_missing_value_falls_back_to_its_closest_ancestor() {
        let index = PathIndex::build(DOC);
        let missing = index.value(&path(&["datasets", "0", "primary_key"]));
        let dataset = index.value(&path(&["datasets", "0"]));
        assert!(missing.is_some());
        assert_eq!(missing, dataset);
    }

    #[test]
    fn top_level_keys_are_listed_in_order() {
        let index = PathIndex::build(DOC);
        assert_eq!(
            index.top_level_keys(),
            ["name", "datasets", "relationships"]
        );
    }

    #[test]
    fn broken_yaml_still_indexes_what_came_before() {
        let index = PathIndex::build("name: ok\nbroken: [unclosed\n");
        assert!(index.value(&path(&["name"])).is_some());
    }
}
