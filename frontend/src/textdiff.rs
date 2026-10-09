//! Line diffs for the config history (what a version changed, what a
//! revert would change), and the small text helpers the editor needs.

/// One line of a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Same(String),
    Added(String),
    Removed(String),
}

/// The lines of `old` and `new`, as a diff: the longest common
/// subsequence kept, everything else removed or added. Config files are
/// small, so the quadratic table is fine.
#[must_use]
pub fn diff(old: &str, new: &str) -> Vec<Line> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // lengths of common subsequences of a[i..] and b[j..]
    let mut table = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(Line::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(Line::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(Line::Added(b[j].to_string()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| Line::Removed((*l).to_string())));
    out.extend(b[j..].iter().map(|l| Line::Added((*l).to_string())));
    out
}

/// Only the changed lines of a diff with `context` unchanged lines around
/// each change; `None` marks lines left out.
#[must_use]
pub fn hunks(lines: &[Line], context: usize) -> Vec<Option<&Line>> {
    let near = |i: usize| {
        let from = i.saturating_sub(context);
        let to = (i + context + 1).min(lines.len());
        lines[from..to].iter().any(|l| !matches!(l, Line::Same(_)))
    };
    let mut out = Vec::new();
    let mut skipped = false;
    for (i, line) in lines.iter().enumerate() {
        if near(i) {
            if skipped {
                out.push(None);
                skipped = false;
            }
            out.push(Some(line));
        } else {
            skipped = true;
        }
    }
    if skipped && !out.is_empty() {
        out.push(None);
    }
    out
}

/// The character offset where 1-based `line` starts in `text` (the end,
/// past the last line): for putting the cursor on a problem.
#[must_use]
pub fn offset_of_line(text: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    text.char_indices()
        .filter(|(_, c)| *c == '\n')
        .nth(line - 2)
        .map_or(text.chars().count(), |(at, _)| text[..=at].chars().count())
}

/// Whether `path` is where a config file can live, as the server checks
/// it: for the "new file" field.
#[must_use]
pub fn config_path(path: &str) -> bool {
    let named = |rest: &str| {
        rest.strip_suffix(".yaml").is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    };
    path == "tidepool.yaml"
        || path == "model.ossie.yaml"
        || path.strip_prefix("tables/").is_some_and(named)
        || path.strip_prefix("dashboards/").is_some_and(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_keeps_common_lines_and_marks_the_rest() {
        let got = diff("a\nb\nc\n", "a\nx\nc\nd\n");
        assert_eq!(
            got,
            vec![
                Line::Same("a".into()),
                Line::Removed("b".into()),
                Line::Added("x".into()),
                Line::Same("c".into()),
                Line::Added("d".into()),
            ]
        );
        assert!(
            diff("same\n", "same\n")
                .iter()
                .all(|l| matches!(l, Line::Same(_)))
        );
        assert_eq!(diff("", "new"), vec![Line::Added("new".into())]);
    }

    #[test]
    fn hunks_keep_context_and_mark_gaps() {
        let old = (0..20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let new = old.replace("10\n", "ten\n");
        let lines = diff(&old, &new);
        let shown = hunks(&lines, 2);
        assert_eq!(shown.first(), Some(&None));
        assert_eq!(shown.last(), Some(&None));
        assert_eq!(shown.iter().filter(|l| l.is_some()).count(), 6);
        assert_eq!(hunks(&diff("a\n", "a\n"), 2), []);
    }

    #[test]
    fn line_offsets_count_characters() {
        let text = "één\ntwo\nthree";
        assert_eq!(offset_of_line(text, 1), 0);
        assert_eq!(offset_of_line(text, 2), 4);
        assert_eq!(offset_of_line(text, 3), 8);
        assert_eq!(offset_of_line(text, 9), text.chars().count());
    }

    #[test]
    fn new_files_go_where_config_files_live() {
        assert!(config_path("tables/returns.yaml"));
        assert!(config_path("dashboards/ops.yaml"));
        assert!(!config_path("tables/returns.yml"));
        assert!(!config_path("tables/../x.yaml"));
        assert!(!config_path("notes.yaml"));
    }
}
