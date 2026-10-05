//! The files `@` completes in the prompt: listing them, ranking them
//! against what was typed, and finding the `@` token under the cursor.

use std::path::Path;

use ignore::WalkBuilder;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// How many files are listed at most; a tree with more is cut off, deepest
/// files first.
pub const LISTED: usize = 50_000;

/// How many matches the prompt offers.
pub const SHOWN: usize = 50;

/// Directories never listed: git's own, and unharness's in the workspace.
const SKIPPED: &[&str] = &[".git", ".unharness"];

/// The files under `root` that its ignore files (`.gitignore`, `.ignore`,
/// git's excludes) leave in, as `/`-separated paths relative to `root`,
/// shallowest first. At most `cap` of them. Hidden files are listed.
///
/// A path that is not UTF-8 or has a control character is left out: it
/// could not be put into the prompt unchanged.
pub fn walk(root: &Path, cap: usize) -> Vec<String> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            !entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIPPED.contains(&name))
        })
        .build();
    let mut paths = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let parts = relative
            .components()
            .map(|c| c.as_os_str().to_str())
            .collect::<Option<Vec<_>>>();
        let Some(parts) = parts else {
            continue;
        };
        let path = parts.join("/");
        if path.chars().any(char::is_control) {
            continue;
        }
        paths.push((parts.len(), path));
    }
    paths.sort();
    paths.truncate(cap);
    paths.into_iter().map(|(_, path)| path).collect()
}

/// The `limit` paths that match `query` best, best first. The match is
/// fuzzy (the query's characters in order, not necessarily adjacent) and
/// ignores case unless the query has an upper-case letter. An empty query
/// keeps the order of `paths`.
pub fn rank(query: &str, paths: &[String], limit: usize) -> Vec<String> {
    if query.is_empty() {
        return paths.iter().take(limit).cloned().collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    // Not `Pattern::parse`: `^`, `$`, `'` and `!` are characters of a path.
    let pattern = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &String)> = paths
        .iter()
        .filter_map(|path| {
            let score = pattern.score(Utf32Str::new(path, &mut buf), &mut matcher)?;
            Some((score, path))
        })
        .collect();
    scored.sort_by(|(a, p), (b, q)| b.cmp(a).then(p.len().cmp(&q.len())).then(p.cmp(q)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path)| path.clone())
        .collect()
}

/// The `@` token the cursor is at the end of: the char index of its `@`
/// and what follows it up to the cursor. The `@` has to start the input or
/// follow whitespace, so an address like `git@host` is not one.
pub fn token_at(input: &str, cursor: usize) -> Option<(usize, String)> {
    let before: Vec<char> = input.chars().take(cursor).collect();
    let start = before
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |i| i + 1);
    if before.get(start) != Some(&'@') {
        return None;
    }
    Some((start, before[start + 1..].iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, path: &str) {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "").unwrap();
    }

    #[test]
    fn walk_lists_what_the_ignore_files_leave_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in [
            "src/tui/app.rs",
            "src/main.rs",
            "README.md",
            ".github/ci.yml",
            "target/debug/app",
            "notes.log",
            ".git/config",
            ".unharness/history",
        ] {
            touch(root, path);
        }
        // This `.git` is no repository: a `.gitignore` counts without one.
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();

        assert_eq!(
            walk(root, 100),
            [
                ".gitignore",
                "README.md",
                ".github/ci.yml",
                "src/main.rs",
                "src/tui/app.rs"
            ]
        );
        assert_eq!(walk(root, 2), [".gitignore", "README.md"]);
    }

    #[test]
    fn walk_reads_the_ignore_files_above_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "crate/src/lib.rs");
        touch(root, "crate/out/lib.o");
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "out/\n").unwrap();

        assert_eq!(walk(&root.join("crate"), 100), ["src/lib.rs"]);
    }

    #[test]
    fn rank_is_fuzzy_and_prefers_the_file_name() {
        let paths: Vec<String> = [
            "docs/application.md",
            "src/tui/app.rs",
            "src/harness/acp/parse.rs",
            "Cargo.toml",
        ]
        .map(String::from)
        .to_vec();

        assert_eq!(rank("", &paths, 2), paths[..2]);
        assert_eq!(rank("apprs", &paths, 10)[0], "src/tui/app.rs");
        assert_eq!(rank("tuiapp", &paths, 10), ["src/tui/app.rs"]);
        assert_eq!(rank("cargo", &paths, 10), ["Cargo.toml"]);
        assert!(rank("CARGO", &paths, 10).is_empty());
        assert!(rank("zzz", &paths, 10).is_empty());
        assert_eq!(rank("rs", &paths, 1).len(), 1);
    }

    #[test]
    fn rank_takes_pattern_characters_literally() {
        let paths = vec!["a/^b$.txt".to_string(), "a/b.txt".to_string()];
        assert_eq!(rank("^b$", &paths, 10), ["a/^b$.txt"]);
        assert_eq!(rank("!b", &paths, 10), Vec::<String>::new());
    }

    #[test]
    fn token_is_an_at_word_ending_at_the_cursor() {
        assert_eq!(token_at("@", 1), Some((0, String::new())));
        assert_eq!(token_at("@src", 4), Some((0, "src".to_string())));
        assert_eq!(token_at("see @sr", 7), Some((4, "sr".to_string())));
        assert_eq!(token_at("one\n@a", 6), Some((4, "a".to_string())));
        assert_eq!(token_at("é @ü", 4), Some((2, "ü".to_string())));
        // The cursor inside the word: what is before it.
        assert_eq!(token_at("@src/tui", 4), Some((0, "src".to_string())));

        assert_eq!(token_at("git@host", 8), None);
        assert_eq!(token_at("see @sr ", 8), None);
        assert_eq!(token_at("see", 3), None);
        assert_eq!(token_at("", 0), None);
    }
}
