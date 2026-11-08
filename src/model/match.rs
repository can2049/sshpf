//! `fuzzy_match`: the case-insensitive subsequence matcher.
//!
//! Used by the host picker and the main table's filter, where the needle is a
//! name the user half-remembers (`dbs` for `db-staging`) rather than a substring
//! they want to scope a column to (that is `import::col_match`).
//!
//! Belongs here: matching rules only.
//! Does not belong: ranking, highlighting, rendering.

/// Case-insensitive subsequence match. An empty (or all-space) needle matches
/// everything; spaces inside the needle are ignored.
pub fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut hay = haystack.chars().flat_map(|c| c.to_lowercase());
    for nc in needle.chars().flat_map(|c| c.to_lowercase()) {
        if nc == ' ' {
            continue;
        }
        loop {
            match hay.next() {
                Some(hc) if hc == nc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("dbs", "db-staging"));
        assert!(fuzzy_match("DB", "db-staging"));
        assert!(!fuzzy_match("zz", "db-staging"));
    }

    #[test]
    fn fuzzy_match_edge_cases() {
        assert!(fuzzy_match("", ""), "empty needle matches empty haystack");
        assert!(fuzzy_match("", "abc"));
        assert!(!fuzzy_match("abc", ""), "non-empty needle cannot match nothing");
        assert!(fuzzy_match("abc", "abc"));
        assert!(fuzzy_match("ac", "abc"), "subsequence, not substring");
        assert!(!fuzzy_match("ca", "abc"), "order matters");
        assert!(fuzzy_match(" ssh ", "my-ssh-host"), "spaces in the needle are ignored");
        assert!(fuzzy_match("SSH", "ssh-box"), "case insensitive");
    }
}
