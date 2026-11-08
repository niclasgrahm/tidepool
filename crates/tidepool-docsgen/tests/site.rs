//! What keeps `website/` from drifting away from the sources it describes.
//!
//! Three ways the site can lie, three checks. The generated reference is
//! committed (so the site builds without Rust), so something has to say when
//! it no longer matches the sources: `just docs` is the fix. A generated
//! partial that no page includes is a reference nobody can read. And a YAML
//! sample on the site is what people copy, so every sample that is a whole
//! config file is compiled, with the retail example around it, by the same
//! compiler the server runs.

use std::path::{Path, PathBuf};

use tidepool_config::ConfigSet;
use tidepool_docsgen::{GENERATED, files, repository};
use tidepool_model::{Catalog, compile};

fn website() -> PathBuf {
    repository().join("website")
}

/// Every markdown page of the site, generated partials excluded.
fn pages() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            panic!("{} can't be read", dir.display())
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !matches!(
                    name.as_str(),
                    "node_modules" | ".vitepress" | "public" | "generated"
                ) {
                    walk(&path, out);
                }
            } else if is_markdown(&path) && name != "readme.md" {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&website(), &mut out);
    out.sort();
    out
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

fn read(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => panic!("{}: {err}", path.display()),
    }
}

#[test]
fn the_committed_reference_matches_the_sources() {
    let generated = match files() {
        Ok(files) => files,
        Err(err) => panic!("the generator failed: {err}"),
    };
    let root = website();
    let mut stale = Vec::new();
    for file in generated {
        match std::fs::read_to_string(root.join(&file.path)) {
            Ok(committed) if committed == file.contents => {}
            Ok(_) => stale.push(format!("{} has drifted", file.path)),
            Err(_) => stale.push(format!("{} is missing", file.path)),
        }
    }
    assert!(
        stale.is_empty(),
        "the doc site's generated reference is out of date; run `just docs`:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn every_generated_partial_is_included_by_a_page() {
    let generated = match files() {
        Ok(files) => files,
        Err(err) => panic!("the generator failed: {err}"),
    };
    let root = website();
    let mut included = Vec::new();
    for page in pages() {
        let dir = page.parent().map(Path::to_path_buf).unwrap_or_default();
        for line in read(&page).lines() {
            let Some(rest) = line.trim().strip_prefix("<!--@include:") else {
                continue;
            };
            let target = rest.trim_end_matches("-->").trim();
            included.push(normalize(&dir.join(target)));
        }
    }
    let missing: Vec<String> = generated
        .iter()
        .filter(|f| f.path.starts_with(GENERATED) && is_markdown(Path::new(&f.path)))
        .filter(|f| !included.contains(&normalize(&root.join(&f.path))))
        .map(|f| f.path.clone())
        .collect();
    assert!(
        missing.is_empty(),
        "generated partials that no page includes (add an `<!--@include: -->` to a page):\n  {}",
        missing.join("\n  ")
    );
}

/// A path with `.` and `..` resolved, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A fenced `yaml` block, and the line of the page it starts on.
struct Sample {
    line: usize,
    text: String,
}

fn yaml_blocks(page: &str) -> Vec<Sample> {
    let mut blocks = Vec::new();
    let mut current: Option<Sample> = None;
    for (n, line) in page.lines().enumerate() {
        let fence = line.trim();
        match &mut current {
            None if fence == "```yaml" => {
                current = Some(Sample {
                    line: n + 2,
                    text: String::new(),
                });
            }
            Some(_) if fence == "```" => blocks.extend(current.take()),
            Some(sample) => {
                sample.text.push_str(line);
                sample.text.push('\n');
            }
            None => {}
        }
    }
    blocks
}

/// Every `yaml` sample is either marked `# fragment` on its first line
/// (`# fragment: kayak's config.yaml` names what it is a piece of), or
/// names the config file it is (`# tables/orders.yaml`) and compiles in place
/// of that file in the retail example.
#[test]
fn every_whole_yaml_sample_compiles_in_the_retail_example() {
    let retail = repository().join("examples/retail");
    let (base, _) = match ConfigSet::load_dir(&retail) {
        Ok(loaded) => loaded,
        Err(err) => panic!("reading the retail example: {err}"),
    };
    let base: Vec<(PathBuf, String)> = base
        .files
        .iter()
        .map(|f| (f.path.clone(), f.text.clone()))
        .collect();

    let mut problems = Vec::new();
    let mut compiled = 0;
    for page in pages() {
        let name = page
            .strip_prefix(website())
            .unwrap_or(&page)
            .display()
            .to_string();
        for sample in yaml_blocks(&read(&page)) {
            let first = sample.text.lines().next().unwrap_or_default().trim();
            let Some(header) = first.strip_prefix('#').map(str::trim) else {
                problems.push(format!(
                    "{name}:{}: a yaml sample must start with `# fragment` or `# <its file>`",
                    sample.line
                ));
                continue;
            };
            // `# fragment`, or `# fragment: what it is` (kayak's config)
            if header.starts_with("fragment") {
                continue;
            }
            let file = PathBuf::from(header);
            let mut files: Vec<(PathBuf, String)> =
                base.iter().filter(|(p, _)| *p != file).cloned().collect();
            files.push((file, sample.text.clone()));
            let (set, mut diagnostics) = ConfigSet::from_files(files);
            let (_, more) = compile(&set, &Catalog::default());
            diagnostics.extend(more);
            if diagnostics.has_errors() {
                problems.push(format!(
                    "{name}:{}: `{header}` doesn't compile in the retail example:\n{}",
                    sample.line,
                    tidepool_config::diag::render_all(&diagnostics, &set.files)
                ));
            }
            compiled += 1;
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(
        compiled > 0,
        "no whole yaml sample on the site was compiled"
    );
}

/// The agents page lists the tools by hand; a tool it leaves out is one a
/// reader never hears of.
#[test]
fn the_agents_page_lists_every_mcp_tool() {
    let page = read(&website().join("agents.md"));
    let tools = tidepool_server::mcp_tools();
    let Some(tools) = tools.as_array() else {
        panic!("tools/list should be a list: {tools}")
    };
    for tool in tools {
        let Some(name) = tool["name"].as_str() else {
            panic!("a tool without a name: {tool}")
        };
        assert!(
            page.contains(&format!("| `{name}` |")),
            "website/agents.md has no row for the `{name}` tool"
        );
    }
}
