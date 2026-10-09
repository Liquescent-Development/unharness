//! The section of `CHANGELOG.md` for a version is its GitHub release's
//! notes (dist reads it when the tag is pushed), so a version bump comes
//! with one.

#[test]
fn the_changelog_has_a_section_for_this_version() {
    let changelog = include_str!("../CHANGELOG.md");
    let version = env!("CARGO_PKG_VERSION");
    let heading = format!("## {version} - ");
    let section = changelog
        .split_once(&heading)
        .map(|(_, rest)| rest.split("\n## ").next().unwrap_or(rest))
        .unwrap_or_else(|| panic!("CHANGELOG.md has no `{heading}<date>` section"));
    let mut lines = section.lines();
    let date = lines.next().unwrap_or("");
    assert!(
        date.len() == 10 && date.chars().all(|c| c.is_ascii_digit() || c == '-'),
        "`{heading}{date}`: the date is not YYYY-MM-DD"
    );
    assert!(
        lines.any(|l| l.starts_with("- ")),
        "the {version} section lists no changes"
    );
}
