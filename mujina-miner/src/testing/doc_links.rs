//! Checks markdown documents for broken internal links.
//!
//! A document passes when:
//!
//! - every reference-style link has a definition
//! - every definition is used by a link
//! - every definition that targets an in-document anchor names one
//!   of the document's headings
//! - every footnote marker has a footnote definition, and every
//!   footnote definition has a marker
//!
//! Put a document under check with a test beside it in the module
//! that owns it.

use std::collections::{BTreeMap, BTreeSet};

/// Returns one message per broken link relationship in `doc`.
pub(crate) fn check(doc: &str) -> Vec<String> {
    let body = without_fenced_blocks(doc);
    let anchors = heading_anchors(&body);

    // Link definitions and footnote definitions claim whole lines;
    // everything else, including a footnote definition's text after
    // its label, is prose that may hold references.
    let mut definitions: BTreeMap<String, String> = BTreeMap::new();
    let mut footnote_definitions: BTreeSet<String> = BTreeSet::new();
    let mut prose = String::new();
    for line in body.lines() {
        if let Some((name, text)) = footnote_definition(line) {
            footnote_definitions.insert(name);
            prose.push_str(text);
        } else if let Some((label, target)) = link_definition(line) {
            definitions.insert(normalize(&label), target);
        } else {
            prose.push_str(line);
        }
        prose.push('\n');
    }
    let (references, footnote_marks) = references(&prose);

    let mut problems = Vec::new();
    for reference in &references {
        if !definitions.contains_key(reference) {
            problems.push(format!("[{reference}] has no definition"));
        }
    }
    for (label, target) in &definitions {
        if !references.contains(label) {
            problems.push(format!("definition [{label}] is never used"));
        }
        if let Some(anchor) = target.strip_prefix('#')
            && !anchors.contains(anchor)
        {
            problems.push(format!(
                "definition [{label}] targets #{anchor}, which \
                 matches no heading"
            ));
        }
    }
    for mark in &footnote_marks {
        if !footnote_definitions.contains(mark) {
            problems.push(format!("footnote [^{mark}] has no definition"));
        }
    }
    for name in &footnote_definitions {
        if !footnote_marks.contains(name) {
            problems.push(format!("footnote [^{name}] is never referenced"));
        }
    }
    problems
}

/// Collects reference-style link labels and footnote names used in
/// prose, both normalized.
///
/// A bare `[label]` counts as a reference only when it contains a
/// letter: bracketed literals like a table's `Data[4]` are not
/// links, and no heading or source label is purely numeric.
fn references(prose: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut references = BTreeSet::new();
    let mut footnotes = BTreeSet::new();
    let chars: Vec<char> = prose.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '[' {
            i += 1;
            continue;
        }
        let Some(close) = matching_close(&chars, i) else {
            i += 1;
            continue;
        };
        let inner: String = chars[i + 1..close].iter().collect();
        if let Some(name) = inner.strip_prefix('^') {
            footnotes.insert(name.to_string());
            i = close + 1;
            continue;
        }
        match chars.get(close + 1) {
            // Inline link: the target is in the parentheses, not a
            // definition. Skip it.
            Some('(') => i = close + 1,
            // Two-part reference: the second label is the reference.
            Some('[') => {
                if let Some(second_close) = matching_close(&chars, close + 1) {
                    let label: String = chars[close + 2..second_close].iter().collect();
                    references.insert(normalize(&label));
                    i = second_close + 1;
                } else {
                    i = close + 1;
                }
            }
            _ => {
                if inner.chars().any(|c| c.is_ascii_alphabetic()) {
                    references.insert(normalize(&inner));
                }
                i = close + 1;
            }
        }
    }
    (references, footnotes)
}

/// Finds the `]` closing the `[` at `open`, giving up at a nested
/// `[` or a line break inside the label beyond the first.
fn matching_close(chars: &[char], open: usize) -> Option<usize> {
    let mut newlines = 0;
    for (offset, c) in chars[open + 1..].iter().enumerate() {
        match c {
            ']' => return Some(open + 1 + offset),
            '[' => return None,
            '\n' => {
                newlines += 1;
                if newlines > 1 {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses a `[^name]: text` footnote definition line.
fn footnote_definition(line: &str) -> Option<(String, &str)> {
    let rest = line.strip_prefix("[^")?;
    let (name, text) = rest.split_once("]:")?;
    Some((name.to_string(), text))
}

/// Parses a `[label]: target` link definition line.
fn link_definition(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix('[')?;
    let (label, text) = rest.split_once("]:")?;
    let target = text.split_whitespace().next()?;
    Some((label.to_string(), target.to_string()))
}

/// Returns the anchors GitHub generates for a document's headings,
/// with `-1`, `-2` suffixes deduplicating repeated headings.
fn heading_anchors(body: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchors = BTreeSet::new();
    for line in body.lines() {
        if !line.starts_with('#') {
            continue;
        }
        let text = line.trim_start_matches('#').trim_start();
        let slug = anchor(text);
        let n = seen.entry(slug.clone()).or_insert(0);
        if *n == 0 {
            anchors.insert(slug);
        } else {
            anchors.insert(format!("{slug}-{n}"));
        }
        *n += 1;
    }
    anchors
}

/// Slugs a heading the way GitHub does: lowercase, punctuation
/// dropped, spaces to hyphens, hyphens and underscores kept.
fn anchor(heading: &str) -> String {
    heading
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// Lowercases a label and collapses its whitespace, the CommonMark
/// rule for matching a reference to its definition.
fn normalize(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Drops the contents of fenced code blocks, keeping line count.
fn without_fenced_blocks(doc: &str) -> String {
    let mut kept = String::new();
    let mut in_fence = false;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            kept.push('\n');
            continue;
        }
        if !in_fence {
            kept.push_str(line);
        }
        kept.push('\n');
    }
    kept
}

mod tests {
    use super::check;

    #[test]
    fn clean_document_passes() {
        let doc = r#"
# Alpha

A link (see [Alpha]) and a two-part [detail][beta-note].
A bracketed literal Data[4] is not a link.[^note]

```text
[ignored]: inside a fence
```

## Alpha

The repeated heading above anchors as alpha-1.

[Alpha]: #alpha
[beta-note]: #alpha-1
[^note]: A footnote.
"#;
        let problems = check(doc);
        assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    }

    #[test]
    fn each_broken_relationship_is_reported() {
        let doc = r#"
# Alpha

See [Missing] and [Bad]. A stray [^ghost] marker.

[Unused]: #alpha
[Bad]: #beta
[^orphan]: Never referenced.
"#;
        let problems = check(doc);
        let expected = [
            "[missing] has no definition",
            "definition [bad] targets #beta, which matches no heading",
            "definition [unused] is never used",
            "footnote [^ghost] has no definition",
            "footnote [^orphan] is never referenced",
        ];
        for message in expected {
            assert!(
                problems.iter().any(|p| p == message),
                "missing {message:?} in\n{}",
                problems.join("\n")
            );
        }
        assert_eq!(problems.len(), expected.len());
    }
}
