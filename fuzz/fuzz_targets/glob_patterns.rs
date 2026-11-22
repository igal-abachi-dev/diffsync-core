//! Fuzz target `glob_patterns` (backlog E11-S1): brace expansion, rule parsing and matching with
//! arbitrary pattern text and paths. No panic, no hang, and expansion stays inside its cap.
//!
//! Input: byte 0 = option flags, then the pattern lines and the path, separated by 0x00 bytes
//! (the last part is the path).
#![no_main]

use core_glob::{MAX_EXPANSIONS, MatchOptions, RuleSet, expand_braces};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [flags, rest @ ..] = data else { return };
    let text = String::from_utf8_lossy(rest);
    let mut parts: Vec<&str> = text.split('\0').collect();
    let path = parts.pop().unwrap_or("");
    let lines: Vec<String> = parts.iter().map(|s| (*s).to_string()).collect();
    let opts = MatchOptions {
        case_sensitive: flags & 1 != 0,
        windows_paths: flags & 2 != 0,
        allow_negation: flags & 4 != 0,
    };
    for l in &lines {
        if let Ok(v) = expand_braces(l, MAX_EXPANSIONS) {
            assert!(v.len() <= MAX_EXPANSIONS);
        }
    }
    if let Ok(set) = RuleSet::parse(&lines, &opts) {
        let is_dir = flags & 8 != 0;
        let _ = set.decide(path, is_dir);
        let _ = set.validate();
        let names: Vec<Vec<u16>> = path.split('/').map(|s| s.encode_utf16().collect()).collect();
        let refs: Vec<&[u16]> = names.iter().map(Vec::as_slice).collect();
        let _ = set.decide_names(&refs, is_dir);
        let _ = set.decide_leaf(&refs, is_dir);
    }
});
