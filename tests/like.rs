use veldb::like::Pattern;

fn m(pat: &str, s: &str) -> bool {
    Pattern::compile(pat).matches(s)
}

#[test]
fn literal_and_empty() {
    assert!(m("abc", "abc"));
    assert!(!m("abc", "abcd"));
    assert!(!m("abc", "ab"));
    assert!(m("", ""));
    assert!(!m("", "a"));
    assert!(m("%", ""));
    assert!(m("%", "что угодно"));
}

#[test]
fn percent_positions() {
    assert!(m("a%", "abc"));
    assert!(m("%c", "abc"));
    assert!(m("%b%", "abc"));
    assert!(!m("%d%", "abc"));
    assert!(m("a%c", "abc"));
    assert!(m("a%c", "ac"));
    assert!(m("a%b%c", "axxbyyc"));
    assert!(!m("a%b%c", "axxcyyb"));
    assert!(m("%%a%%", "a"));
}

#[test]
fn underscore_counts_characters_not_bytes() {
    assert!(m("_", "я"));
    assert!(!m("_", "яя"));
    assert!(m("__", "яя"));
    assert!(m("а_в", "абв"));
    assert!(m("%_%", "x"));
    assert!(!m("%_%", ""));
}

#[test]
fn utf8_literals() {
    assert!(m("%привет%", "ну привет, мир"));
    assert!(!m("%привет%", "ну прив, мир"));
    assert!(m("🐢%", "🐢🐢"));
}

#[test]
fn backtracking_does_not_blow_up() {
    // Патологический для наивной рекурсии вход: должен отработать мгновенно.
    let s = "a".repeat(64);
    assert!(!m("%a%a%a%a%a%a%a%b", &s));
    assert!(m("%a%a%a%a%a%a%a%a", &s));
}

#[test]
fn anchored_suffix_shorter_than_pattern() {
    assert!(!m("%abcdef", "ef"));
    assert!(m("%ef", "abcdef"));
}
