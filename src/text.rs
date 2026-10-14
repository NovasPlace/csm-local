//! Small string helpers that respect UTF-8 boundaries.

const ELLIPSIS: &str = "…";

/// Cuts `s` to at most `max_bytes`, ending with an ellipsis when shortened.
pub fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes.saturating_sub(ELLIPSIS.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", s[..end].trim_end())
}

/// Keeps the last `max_bytes` of `s`, starting with an ellipsis when shortened.
pub fn tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes.saturating_sub(ELLIPSIS.len());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("{ELLIPSIS}{}", s[start..].trim_start())
}

/// Collapses all whitespace, including newlines, to single spaces.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 8), "abcde…");
        let s = "ééééé";
        let t = truncate(s, 7);
        assert!(t.len() <= 7, "{t}");
        assert!(t.ends_with('…'));
    }

    #[test]
    fn tails_on_char_boundaries() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("abcdefghij", 8), "…fghij");
        let t = tail("ééééé", 7);
        assert!(t.len() <= 7, "{t}");
        assert!(t.starts_with('…'));
    }

    #[test]
    fn flattens_whitespace() {
        assert_eq!(one_line("  a\n\tb   c \r\n"), "a b c");
    }
}
