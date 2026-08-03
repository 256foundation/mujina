//! Consistency tests for the chip reference document.

use crate::testing::doc_links;

#[test]
fn reference_links_resolve() {
    let problems = doc_links::check(include_str!("REFERENCE.md"));
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
