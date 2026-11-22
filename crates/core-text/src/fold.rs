//! Unicode *simple* case folding (decision D5, backlog E1-S4.2).
//!
//! One code point in, one code point out, so folding never changes a text's length in code
//! points and intraline offsets map 1:1 onto the original line. The table is generated at
//! build time from `data/CaseFolding.txt` (statuses `C` and `S`). Never use
//! `str::to_lowercase`: it applies *full* mappings, is context sensitive (final sigma) and can
//! change the length (`İ` becomes two code points).

include!(concat!(env!("OUT_DIR"), "/fold_table.rs"));

/// The simple case folding of `c`; `c` itself when it has none.
#[inline]
pub fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let cp = c as u32;
    match FOLD.binary_search_by_key(&cp, |&(from, _)| from) {
        // The generated table only contains valid scalar values.
        Ok(i) => char::from_u32(FOLD[i].1).unwrap_or(c),
        Err(_) => c,
    }
}

/// True when folding would change at least one character of `s`.
pub fn needs_fold(s: &str) -> bool {
    s.chars().any(|c| fold_char(c) != c)
}

/// Appends the case-folded `s` to `out`.
pub fn fold_into(s: &str, out: &mut String) {
    out.reserve(s.len());
    for c in s.chars() {
        out.push(fold_char(c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_folds_to_lowercase() {
        assert_eq!(fold_char('A'), 'a');
        assert_eq!(fold_char('Z'), 'z');
        assert_eq!(fold_char('a'), 'a');
        assert_eq!(fold_char('5'), '5');
        assert_eq!(fold_char('['), '[');
    }

    #[test]
    fn turkish_dotted_capital_i_is_left_alone() {
        // `İ` has only full (F) and Turkic (T) foldings: simple folding leaves it unchanged,
        // so the length in code points never changes (D5).
        assert_eq!(fold_char('İ'), 'İ');
        assert_eq!(fold_char('ı'), 'ı');
        let mut out = String::new();
        fold_into("İI", &mut out);
        assert_eq!(out, "İi");
    }

    #[test]
    fn sharp_s_does_not_expand() {
        assert_eq!(fold_char('ß'), 'ß');
        // U+1E9E CAPITAL SHARP S has the simple folding ß (status S).
        assert_eq!(fold_char('ẞ'), 'ß');
        let mut out = String::new();
        fold_into("Straße", &mut out);
        assert_eq!(out, "straße");
    }

    #[test]
    fn greek_sigma_forms_fold_together() {
        assert_eq!(fold_char('Σ'), 'σ');
        assert_eq!(fold_char('ς'), 'σ'); // final sigma folds to sigma (status C)
        let (mut a, mut b) = (String::new(), String::new());
        fold_into("ΣΑΣ", &mut a);
        fold_into("σας", &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn other_scripts() {
        assert_eq!(fold_char('Д'), 'д'); // Cyrillic
        assert_eq!(fold_char('É'), 'é');
        assert_eq!(fold_char('\u{212A}'), 'k'); // KELVIN SIGN folds to k (3 bytes -> 1 byte)
        assert_eq!(fold_char('\u{B5}'), '\u{3BC}'); // MICRO SIGN -> GREEK SMALL LETTER MU
        assert_eq!(fold_char('א'), 'א'); // Hebrew has no case
        assert_eq!(fold_char('😀'), '😀');
    }

    #[test]
    fn folding_is_idempotent_over_the_whole_table() {
        for &(from, to) in FOLD {
            let c = char::from_u32(from).expect("scalar");
            let f = fold_char(c);
            assert_eq!(f as u32, to);
            assert_eq!(fold_char(f), f, "U+{from:04X} folds to a fixed point");
        }
    }

    #[test]
    fn table_is_sorted_and_comes_from_the_data_file() {
        assert!(FOLD.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(
            FOLD.len() > 1400,
            "expected ~1500 C/S entries, got {}",
            FOLD.len()
        );
        assert!(
            UNICODE_VERSION.starts_with(|c: char| c.is_ascii_digit()),
            "{UNICODE_VERSION}"
        );
    }

    #[test]
    fn needs_fold_detects_changes() {
        assert!(needs_fold("Hello"));
        assert!(!needs_fold("hello שלום 123"));
        assert!(!needs_fold(""));
    }
}
