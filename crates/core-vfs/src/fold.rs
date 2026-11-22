//! Name folding for case-insensitive comparison (decision D6, plan.md §7.3): NTFS-style
//! uppercase **per UTF-16 unit**. A unit that uppercases to exactly one BMP unit is replaced;
//! anything else (`ß`, `İ` in the Turkic sense, surrogates) is kept, so `ß` does not pair with `SS`
//! and `İ` does not pair with `i`. NFC normalisation, when wanted, is applied by the caller
//! (`core-folder`) before folding.

/// The uppercase of one UTF-16 unit under the D6 rules.
#[inline]
pub fn fold_unit(u: u16) -> u16 {
    if u < 0x80 {
        return if (u16::from(b'a')..=u16::from(b'z')).contains(&u) {
            u - 32
        } else {
            u
        };
    }
    let Some(c) = char::from_u32(u32::from(u)) else {
        return u;
    }; // lone surrogates stay
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(x), None) if (x as u32) <= 0xFFFF => x as u32 as u16,
        _ => u,
    }
}

/// Folds a whole name.
pub fn fold_name(units: &[u16]) -> Vec<u16> {
    units.iter().map(|&u| fold_unit(u)).collect()
}

/// Case-insensitive equality of two names under D6.
pub fn names_equal_ci(a: &[u16], b: &[u16]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| fold_unit(x) == fold_unit(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Vec<u16> {
        fold_name(&s.encode_utf16().collect::<Vec<_>>())
    }

    #[test]
    fn ascii_and_accents_fold_up() {
        assert_eq!(f("Readme.TXT"), f("README.txt"));
        assert_eq!(f("é"), f("É"));
        assert_eq!(f("σ"), f("Σ"));
    }

    /// The folder golden rows of plan.md §11.3.
    #[test]
    fn sharp_s_and_dotted_i_do_not_pair() {
        assert_ne!(f("ß"), f("SS"), "per-unit uppercase does not expand ß");
        assert_ne!(f("İ"), f("i"), "i uppercases to I, not İ");
        assert_eq!(f("i"), f("I"));
    }

    #[test]
    fn lone_surrogates_and_astral_characters_are_kept() {
        assert_eq!(fold_unit(0xD800), 0xD800);
        let emoji: Vec<u16> = "😀".encode_utf16().collect();
        assert_eq!(fold_name(&emoji), emoji);
        assert!(names_equal_ci(&f("abc"), &f("ABC")) && !names_equal_ci(&f("a"), &f("ab")));
    }
}
