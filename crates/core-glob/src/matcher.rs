//! The segment matcher (replaces `GlobMatcher.GlobSegmentToRegex`, plan.md §7.5, backlog E6-S2).
//!
//! One path segment is compiled into tokens and matched with the classic iterative wildcard
//! algorithm (backtracking only to the last `*`): linear in practice, no regex, no timeout.
//!
//! * `*` matches any run of characters (never `/`: segments contain none), **including a leading
//!   dot** (decision D15); `?` matches one character;
//! * `[abc]`, `[a-z]`, `[!a-z]` / `[^a-z]`; `]` first in a class is a literal, `-` first or last
//!   is a literal; an unterminated `[` is a literal `[`;
//! * `\x` matches `x` literally (the caller turns `\` into `/` first on Windows sources);
//! * case-insensitive matching folds per UTF-16 unit (decision D6).
//!
//! Matching works on UTF-16 code units, the same unit as `core_vfs::Name`.

use core_vfs::fold::fold_unit;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Lit(u16),
    Any,
    One,
    Class {
        negated: bool,
        ranges: Vec<(u16, u16)>,
    },
}

/// A compiled segment pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    tokens: Vec<Token>,
    case_sensitive: bool,
}

fn lower_unit(u: u16) -> u16 {
    char::from_u32(u32::from(u)).map_or(u, |c| {
        let mut l = c.to_lowercase();
        match (l.next(), l.next()) {
            (Some(x), None) if (x as u32) <= 0xFFFF => x as u32 as u16,
            _ => u,
        }
    })
}

impl Segment {
    /// Compiles `pattern`. Unknown syntax is taken literally, never an error.
    pub fn compile(pattern: &str, case_sensitive: bool) -> Segment {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        let lit = |u: u16| Token::Lit(if case_sensitive { u } else { fold_unit(u) });
        while i < units.len() {
            let c = units[i];
            match c {
                0x2A => {
                    // collapse `**` inside a segment to one `*`
                    if tokens.last() != Some(&Token::Any) {
                        tokens.push(Token::Any);
                    }
                }
                0x3F => tokens.push(Token::One),
                0x5C => {
                    if i + 1 < units.len() {
                        i += 1;
                    }
                    tokens.push(lit(units[i]));
                }
                0x5B => match parse_class(&units, i) {
                    Some((tok, next)) => {
                        tokens.push(tok);
                        i = next;
                        continue;
                    }
                    None => tokens.push(lit(c)), // unterminated: a literal `[`
                },
                _ => tokens.push(lit(c)),
            }
            i += 1;
        }
        Segment {
            tokens,
            case_sensitive,
        }
    }

    /// True when the pattern has no wildcard, class or escape: it matches exactly one name.
    pub fn is_literal(&self) -> bool {
        self.tokens.iter().all(|t| matches!(t, Token::Lit(_)))
    }

    /// The literal name (folded when case-insensitive), if the pattern is a plain name.
    pub fn literal_units(&self) -> Option<Vec<u16>> {
        self.tokens
            .iter()
            .map(|t| {
                if let Token::Lit(u) = t {
                    Some(*u)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Whether `name` (UTF-16 units) matches the whole pattern.
    pub fn matches(&self, name: &[u16]) -> bool {
        let toks = &self.tokens;
        let (mut t, mut n) = (0usize, 0usize);
        let (mut star_t, mut star_n) = (usize::MAX, 0usize);
        while n < name.len() {
            if t < toks.len() {
                match &toks[t] {
                    Token::Any => {
                        star_t = t;
                        star_n = n;
                        t += 1;
                        continue;
                    }
                    tok if self.one(tok, name[n]) => {
                        t += 1;
                        n += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            if star_t == usize::MAX {
                return false;
            }
            // backtrack: let the last `*` swallow one more unit
            star_n += 1;
            n = star_n;
            t = star_t + 1;
        }
        toks[t..].iter().all(|t| *t == Token::Any)
    }

    fn one(&self, tok: &Token, u: u16) -> bool {
        match tok {
            Token::Lit(l) => *l == if self.case_sensitive { u } else { fold_unit(u) },
            Token::Class { negated, ranges } => {
                let hit = |c: u16| ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi);
                let inside = if self.case_sensitive {
                    hit(u)
                } else {
                    hit(u) || hit(fold_unit(u)) || hit(lower_unit(u))
                };
                inside != *negated
            }
            Token::One | Token::Any => true,
        }
    }
}

/// Parses `[...]` starting at `units[start] == '['`. Returns the token and the index after
/// `]`, or `None` when there is no closing bracket.
fn parse_class(units: &[u16], start: usize) -> Option<(Token, usize)> {
    let mut j = start + 1;
    let negated = matches!(units.get(j), Some(&0x21 | &0x5E)); // `!` or `^`
    if negated {
        j += 1;
    }
    let mut items: Vec<u16> = Vec::new(); // chars, with 0xFFFF_ marker handled via a parallel flag
    let mut dash_after: Vec<bool> = Vec::new(); // item i is followed by a range dash
    let mut first = true;
    loop {
        let &c = units.get(j)?;
        if c == 0x5D && !first {
            break; // closing bracket (a `]` right after `[` or `[!` is a literal)
        }
        first = false;
        if c == 0x5C {
            j += 1;
            items.push(*units.get(j)?);
            dash_after.push(false);
            j += 1;
            continue;
        }
        // a range dash: not first, and not right before the closing bracket
        if c == 0x2D && !items.is_empty() && units.get(j + 1).is_some_and(|&n| n != 0x5D) {
            if let Some(last) = dash_after.last_mut() {
                *last = true;
            }
            j += 1;
            continue;
        }
        items.push(c);
        dash_after.push(false);
        j += 1;
    }
    let mut ranges = Vec::new();
    let mut k = 0;
    while k < items.len() {
        if dash_after[k] && k + 1 < items.len() {
            let (lo, hi) = (items[k], items[k + 1]);
            ranges.push((lo.min(hi), lo.max(hi)));
            k += 2;
        } else {
            ranges.push((items[k], items[k]));
            k += 1;
        }
    }
    Some((Token::Class { negated, ranges }, j + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, name: &str) -> bool {
        Segment::compile(pat, true).matches(&name.encode_utf16().collect::<Vec<_>>())
    }

    fn mi(pat: &str, name: &str) -> bool {
        Segment::compile(pat, false).matches(&name.encode_utf16().collect::<Vec<_>>())
    }

    /// The cases of `GlobSegmentToRegex` and the §11.3 filter golden rows.
    #[test]
    fn star_question_and_literals() {
        assert!(m("*.obj", "a.obj") && !m("*.obj", "a.obj.bak"));
        assert!(m("img_?.png", "img_1.png") && !m("img_?.png", "img_12.png"));
        assert!(m("*", "anything") && m("*", "") && m("a*", "a") && m("*a", "a"));
        assert!(m("a*b*c", "aXXbYYc") && !m("a*b*c", "aXXbYY"));
        assert!(m("README", "README") && !m("README", "readme"));
        assert!(m("", "") && !m("", "a"));
    }

    #[test]
    fn star_matches_a_leading_dot_decision_d15() {
        assert!(m("*", ".gitignore"));
        assert!(m("*.txt", ".hidden.txt"));
        assert!(m("?bc", ".bc"));
    }

    #[test]
    fn classes_and_ranges() {
        assert!(m("file[0-9].txt", "file7.txt") && !m("file[0-9].txt", "fileA.txt"));
        assert!(m("[abc]x", "bx") && !m("[abc]x", "dx"));
        assert!(m("[a-cx-z]", "y") && !m("[a-cx-z]", "m"));
        assert!(m("[z-a]", "m"), "a reversed range is normalised");
    }

    #[test]
    fn negated_classes() {
        assert!(!m("[!a]*", "abc") && m("[!a]*", "xbc"));
        assert!(m("[^0-9]", "x") && !m("[^0-9]", "5"));
    }

    #[test]
    fn bracket_and_dash_literals() {
        assert!(m("[]]x", "]x"), "`]` first is a literal");
        assert!(
            m("[a-]", "-") && m("[a-]", "a") && !m("[a-]", "b"),
            "trailing `-` is a literal"
        );
        assert!(m("[-a]", "-") && m("[-a]", "a"), "leading `-` is a literal");
        assert!(m("[!]]", "x") && !m("[!]]", "]"));
        assert!(m("a[b", "a[b"), "an unterminated class is a literal `[`");
        assert!(m("[", "[") && m("[!", "[!"));
    }

    #[test]
    fn escapes() {
        assert!(m("a\\*b", "a*b") && !m("a\\*b", "aXb"));
        assert!(m("a\\?", "a?") && m("[\\]]", "]"));
        assert!(
            m("trailing\\", "trailing\\"),
            "a lone trailing backslash is a literal, as in the C# matcher"
        );
    }

    #[test]
    fn case_insensitive_folds_per_unit_decision_d6() {
        assert!(mi("README", "readme") && mi("*.TXT", "a.txt"));
        assert!(mi("[a-c]x", "BX"), "classes fold too");
        assert!(mi("É*", "école"));
        assert!(!mi("ß", "SS") && !mi("SS", "ß"), "no expansion of ß");
        assert!(!mi("İ", "i"));
        assert!(!m("README", "readme"));
    }

    #[test]
    fn non_bmp_and_unpaired_surrogates_match_by_unit() {
        let pat = Segment::compile("a?c", true);
        // an emoji is two units: `?` matches one unit, so a?c does not match a😀c but a??c does
        let emoji: Vec<u16> = "a😀c".encode_utf16().collect();
        assert!(!pat.matches(&emoji));
        assert!(Segment::compile("a??c", true).matches(&emoji));
        assert!(Segment::compile("a*c", true).matches(&emoji));
        assert!(Segment::compile("*", true).matches(&[0xD800]));
    }

    #[test]
    fn literal_detection() {
        assert!(Segment::compile("abc", true).is_literal());
        assert!(!Segment::compile("a*c", true).is_literal());
        assert_eq!(
            Segment::compile("Ab", false).literal_units(),
            Some(vec![0x41, 0x42])
        );
    }

    #[test]
    fn pathological_patterns_stay_fast() {
        // a classic exponential case for naive backtracking
        let pat = format!("{}b", "a*".repeat(30));
        let name = "a".repeat(2000);
        let t = std::time::Instant::now();
        assert!(!m(&pat, &name));
        assert!(
            t.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
    }
}
