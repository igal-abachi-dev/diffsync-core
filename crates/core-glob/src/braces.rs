//! Brace expansion (port of `GlobPatternParser.ExpandBraces`, plan.md §7.5, backlog E6-S1).
//!
//! `a{b,c}d` → `abd`, `acd`. Braces nest, `\,` `\{` `\}` are escapes, and a block with one
//! option (`{a}`) stays literal. Expansion happens **before** separator normalisation, as in
//! Glob. More than [`MAX_EXPANSIONS`] results is a `PatternError` (`{a,b}` thirteen times
//! would give 8192).
//!
//! One difference from the C# code: it stops looking after a one-option block, so `{a}{b,c}`
//! is not expanded there. Here the literal block is kept and later blocks still expand.

use std::fmt;

/// Cap on the number of expansions of one pattern.
pub const MAX_EXPANSIONS: usize = 4096;

/// An error in a pattern: too many expansions or invalid syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternError(pub String);

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pattern error: {}", self.0)
    }
}

impl std::error::Error for PatternError {}

impl From<PatternError> for core_base::Error {
    fn from(e: PatternError) -> Self {
        core_base::Error::invalid_argument(e.0)
    }
}

/// Expands braces; at most `limit` results (use [`MAX_EXPANSIONS`]).
pub fn expand_braces(pattern: &str, limit: usize) -> Result<Vec<String>, PatternError> {
    let mut out = Vec::new();
    expand(pattern, limit, &mut out)?;
    Ok(out.into_iter().map(|s| unescape(&s)).collect())
}

fn unescape(s: &str) -> String {
    s.replace("\\{", "{")
        .replace("\\}", "}")
        .replace("\\,", ",")
}

fn expand(pattern: &str, limit: usize, out: &mut Vec<String>) -> Result<(), PatternError> {
    let chars: Vec<char> = pattern.chars().collect();
    let (mut first, mut level) = (None, 0usize);
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '{' => {
                if level == 0 {
                    first = Some(i);
                }
                level += 1;
            }
            '}' if level > 0 => {
                level -= 1;
                if let (0, Some(start)) = (level, first) {
                    let prefix: String = chars[..start].iter().collect();
                    let body: String = chars[start + 1..i].iter().collect();
                    let suffix: String = chars[i + 1..].iter().collect();
                    let options = split_top_level(&body);
                    if options.len() == 1 {
                        if body.contains("\\,") {
                            // `{a\,b}`: one option holding an escaped comma; the braces go away
                            // and the comma becomes literal (golden row `img_{a\,b}.png`)
                            return expand(&format!("{prefix}{body}{suffix}"), limit, out);
                        }
                        // `{a}`: no comma at all: the block stays literal. Escape its braces so
                        // later blocks can still expand and the final unescape restores them.
                        let literal = format!("{prefix}\\{{{body}\\}}{suffix}");
                        return expand(&literal, limit, out);
                    }
                    for opt in options {
                        expand(&format!("{prefix}{opt}{suffix}"), limit, out)?;
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
        i += 1;
    }
    if out.len() >= limit {
        return Err(PatternError(format!(
            "brace expansion produces more than {limit} patterns: `{pattern}`"
        )));
    }
    out.push(pattern.to_string());
    Ok(())
}

fn split_top_level(body: &str) -> Vec<String> {
    let (mut out, mut cur, mut level) = (Vec::new(), String::new(), 0i32);
    let mut it = body.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = it.next() {
                    cur.push(n);
                }
            }
            '{' => {
                level += 1;
                cur.push(c);
            }
            '}' => {
                level -= 1;
                cur.push(c);
            }
            ',' if level == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(p: &str) -> Vec<String> {
        expand_braces(p, MAX_EXPANSIONS).unwrap()
    }

    #[test]
    fn simple_and_multiple_blocks() {
        assert_eq!(ex("*.{jpg,png}"), ["*.jpg", "*.png"]);
        assert_eq!(ex("a{b,c}d{e,f}"), ["abde", "abdf", "acde", "acdf"]);
        assert_eq!(
            ex("src/{api,client}/v{1,2}/*"),
            [
                "src/api/v1/*",
                "src/api/v2/*",
                "src/client/v1/*",
                "src/client/v2/*"
            ]
        );
    }

    #[test]
    fn nested_braces() {
        assert_eq!(ex("{a,b{c,d}}"), ["a", "bc", "bd"]);
        assert_eq!(ex("x{a,{b,c}d}y"), ["xay", "xbdy", "xcdy"]);
    }

    #[test]
    fn escapes_inside_braces() {
        assert_eq!(
            ex("img_{a\\,b}.png"),
            ["img_a,b.png"],
            "escaped comma: one option, literal"
        );
        assert_eq!(ex("{a\\,b,c}"), ["a,b", "c"]);
        assert_eq!(ex("\\{a,b\\}"), ["{a,b}"], "escaped braces are not a block");
    }

    #[test]
    fn one_option_braces_stay_literal() {
        assert_eq!(ex("{a}.txt"), ["{a}.txt"]);
        assert_eq!(ex("{}"), ["{}"]);
        // later blocks still expand (the C# code stopped here)
        assert_eq!(ex("{a}{b,c}"), ["{a}b", "{a}c"]);
    }

    #[test]
    fn unbalanced_braces_are_literal() {
        assert_eq!(ex("{a,b"), ["{a,b"]);
        assert_eq!(ex("a}b"), ["a}b"]);
        assert_eq!(ex("no braces"), ["no braces"]);
        assert_eq!(ex(""), [""]);
    }

    #[test]
    fn empty_options_are_allowed() {
        assert_eq!(ex("a{,b}"), ["a", "ab"]);
    }

    /// Backlog E6-S1.2: `{a,b}` thirteen times is 8192 > 4096 → error; twelve is exactly 4096.
    #[test]
    fn the_expansion_cap() {
        let twelve = "{a,b}".repeat(12);
        assert_eq!(ex(&twelve).len(), 4096);
        let thirteen = "{a,b}".repeat(13);
        let e = expand_braces(&thirteen, MAX_EXPANSIONS).unwrap_err();
        assert!(e.0.contains("4096"), "{e}");
        assert_eq!(
            core_base::Error::from(e).kind(),
            core_base::ErrorKind::InvalidArgument
        );
        assert!(expand_braces("{a,b,c}", 2).is_err());
    }
}
