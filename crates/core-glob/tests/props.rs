//! Property tests of the segment matcher and rules (backlog E6-S2.2, E6-S3).

use core_glob::{MatchOptions, RuleSet, Segment};
use proptest::prelude::*;

/// A naive recursive wildcard matcher over `*`, `?` and literal characters: the reference.
fn naive(pat: &[char], s: &[char]) -> bool {
    match pat.split_first() {
        None => s.is_empty(),
        Some(('*', rest)) => (0..=s.len()).any(|k| naive(rest, &s[k..])),
        Some(('?', rest)) => !s.is_empty() && naive(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && naive(rest, &s[1..]),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 10_000, ..ProptestConfig::default() })]

    /// For patterns of `*`, `?` and letters the iterative matcher agrees with the naive one.
    #[test]
    fn iterative_matcher_equals_the_naive_one(pat in "[ab*?]{0,9}", name in "[ab]{0,12}") {
        let seg = Segment::compile(&pat, true);
        let units: Vec<u16> = name.encode_utf16().collect();
        let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
        prop_assert_eq!(seg.matches(&units), naive(&p, &n), "pattern {:?} name {:?}", pat, name);
    }

    /// Case-insensitive matching equals matching the upper-cased pattern against the upper-cased
    /// name (ASCII letters).
    #[test]
    fn case_insensitive_equals_matching_both_folded(pat in "[aAbB*?]{0,8}", name in "[aAbB]{0,10}") {
        let ci = Segment::compile(&pat, false).matches(&name.encode_utf16().collect::<Vec<_>>());
        let cs = Segment::compile(&pat.to_uppercase(), true).matches(&name.to_uppercase().encode_utf16().collect::<Vec<_>>());
        prop_assert_eq!(ci, cs);
    }

    /// A pattern made only of a literal name matches exactly that name.
    #[test]
    fn literals_match_only_themselves(name in "[a-d.]{1,8}", other in "[a-d.]{1,8}") {
        let seg = Segment::compile(&name, true);
        prop_assert!(seg.matches(&name.encode_utf16().collect::<Vec<_>>()));
        prop_assert_eq!(seg.matches(&other.encode_utf16().collect::<Vec<_>>()), name == other);
    }

    /// The decision never panics, and a path inside an excluded folder is excluded.
    #[test]
    fn rules_never_panic_and_ancestors_dominate(
        rules in proptest::collection::vec("!?[a-c*?]{1,3}(/[a-c*]{1,2})?/?", 0..5),
        path in proptest::collection::vec("[a-c]{1,3}", 1..5),
        is_dir in any::<bool>(),
    ) {
        let o = MatchOptions { case_sensitive: true, windows_paths: true, allow_negation: true };
        let Ok(set) = RuleSet::parse(&rules, &o) else { return Ok(()) };
        let joined = path.join("/");
        let d = set.decide(&joined, is_dir);
        // an excluded ancestor folder excludes the path, whatever the rules say about it
        for k in 1..path.len() {
            if set.decide(&path[..k].join("/"), true).is_excluded() {
                prop_assert!(d.is_excluded(), "ancestor {:?} excluded but {:?} is {:?}", &path[..k], joined, d);
            }
        }
        let _ = set.validate();
    }
}
