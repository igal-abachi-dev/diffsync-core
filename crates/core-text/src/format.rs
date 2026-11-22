//! Built-in comment presets per file type (decision D9, backlog E1-S5).
//!
//! A [`FileFormat`] maps file extensions to a list of "unimportant text" regexes, so
//! comment-only changes show as *unimportant* out of the box. Presets are applied to
//! [`LineRules`] per session and can be switched off.
//!
//! # Known v1 limitations (D9)
//!
//! * Comment markers inside **string literals** are not understood: `"http://x"` loses
//!   `//x"`, `"a # b"` loses `# b"`. The result only affects the *importance* of a
//!   difference, never alignment correctness of identical lines.
//! * Comments that **span several lines** need lexer state and are not handled; only
//!   same-line `/* … */` and `<!-- … -->` are removed.

use core_base::Result;

use crate::rules::LineRules;

/// A comment family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFormat {
    pub name: &'static str,
    /// Lowercase extensions without the dot.
    pub extensions: &'static [&'static str],
    /// Regexes removed from each line, applied in order.
    pub unimportant: &'static [&'static str],
}

/// `//` line comments and same-line `/* … */` (block comments first, so `// /* x */` is
/// still one line comment).
const C_FAMILY: FileFormat = FileFormat {
    name: "C-family",
    extensions: &[
        "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "cs", "java", "js", "jsx", "mjs", "cjs",
        "ts", "tsx", "rs", "go", "swift", "kt", "kts", "scala", "dart", "php", "m", "mm", "proto",
    ],
    unimportant: &["/\\*.*?\\*/", "//.*$"],
};

const HASH: FileFormat = FileFormat {
    name: "hash",
    extensions: &[
        "py",
        "pyw",
        "sh",
        "bash",
        "zsh",
        "yml",
        "yaml",
        "toml",
        "ps1",
        "psm1",
        "psd1",
        "rb",
        "pl",
        "pm",
        "r",
        "mk",
        "cmake",
        "dockerfile",
        "gitignore",
        "conf",
        "properties",
    ],
    unimportant: &["#.*$"],
};

const DASH: FileFormat = FileFormat {
    name: "double-dash",
    extensions: &["sql", "lua", "hs", "elm", "ada", "adb", "ads"],
    unimportant: &["--.*$"],
};

const SEMICOLON: FileFormat = FileFormat {
    name: "semicolon",
    extensions: &["ini", "asm", "s", "inc", "cfg", "reg", "lisp", "clj", "el"],
    unimportant: &[";.*$"],
};

/// `REM text` and `:: text` lines, case-insensitive, at the start of the line.
const BATCH: FileFormat = FileFormat {
    name: "batch",
    extensions: &["bat", "cmd"],
    unimportant: &["(?i)^\\s*rem(\\s.*)?$", "^\\s*::.*$"],
};

const MARKUP: FileFormat = FileFormat {
    name: "markup",
    extensions: &[
        "xml", "html", "htm", "xhtml", "svg", "xaml", "csproj", "props", "targets", "vcxproj",
        "config",
    ],
    unimportant: &["<!--.*?-->"],
};

/// Every built-in format.
pub const FORMATS: &[FileFormat] = &[C_FAMILY, HASH, DASH, SEMICOLON, BATCH, MARKUP];

impl FileFormat {
    /// The format for a file extension (case-insensitive, with or without the dot).
    pub fn for_extension(ext: &str) -> Option<&'static FileFormat> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        FORMATS
            .iter()
            .find(|f| f.extensions.contains(&ext.as_str()))
    }

    /// The format for a file name or path, from its extension (or the whole name for
    /// dot-files such as `.gitignore`).
    pub fn for_path(path: &str) -> Option<&'static FileFormat> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        match name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => Self::for_extension(ext),
            _ => Self::for_extension(name.trim_start_matches('.')),
        }
    }

    /// `base` plus this format's unimportant-text patterns.
    pub fn apply(&self, base: LineRules) -> Result<LineRules> {
        base.with_unimportant(self.unimportant.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Scratch;

    fn norm(fmt: &FileFormat, line: &str) -> String {
        let rules = fmt.apply(LineRules::new()).unwrap();
        rules.normalize(line, &mut Scratch::new()).to_owned()
    }

    #[test]
    fn lookup_by_extension_and_path() {
        assert_eq!(FileFormat::for_extension("rs").unwrap().name, "C-family");
        assert_eq!(FileFormat::for_extension(".PY").unwrap().name, "hash");
        assert_eq!(
            FileFormat::for_extension("SQL").unwrap().name,
            "double-dash"
        );
        assert_eq!(FileFormat::for_extension("ini").unwrap().name, "semicolon");
        assert_eq!(FileFormat::for_extension("cmd").unwrap().name, "batch");
        assert_eq!(FileFormat::for_extension("html").unwrap().name, "markup");
        assert!(FileFormat::for_extension("txt").is_none());
        assert!(FileFormat::for_extension("").is_none());
        assert_eq!(
            FileFormat::for_path("C:\\src\\main.RS").unwrap().name,
            "C-family"
        );
        assert_eq!(FileFormat::for_path("a/b/.gitignore").unwrap().name, "hash");
        assert!(FileFormat::for_path("Makefile").is_none());
        assert!(FileFormat::for_path("archive.tar.gz").is_none());
    }

    #[test]
    fn no_extension_is_claimed_by_two_formats() {
        let mut seen = std::collections::HashMap::new();
        for f in FORMATS {
            for e in f.extensions {
                assert!(seen.insert(*e, f.name).is_none(), "{e} appears twice");
                assert_eq!(*e, e.to_ascii_lowercase());
            }
        }
    }

    #[test]
    fn every_pattern_compiles() {
        for f in FORMATS {
            f.apply(LineRules::new())
                .unwrap_or_else(|e| panic!("{}: {e}", f.name));
        }
    }

    #[test]
    fn c_family() {
        let f = FileFormat::for_extension("c").unwrap();
        assert_eq!(norm(f, "int x; // trailing"), "int x;");
        assert_eq!(norm(f, "int /* inline */ x;"), "int  x;");
        assert_eq!(norm(f, "  /* only a comment */  "), "");
        assert_eq!(norm(f, "// whole line"), "");
        assert_eq!(norm(f, "code(); // has /* a block */ marker"), "code();");
    }

    #[test]
    fn hash_family() {
        let f = FileFormat::for_extension("py").unwrap();
        assert_eq!(norm(f, "x = 1  # set x"), "x = 1");
        assert_eq!(norm(f, "# comment"), "");
        assert_eq!(norm(f, "key: value # yaml"), "key: value");
    }

    #[test]
    fn double_dash_family() {
        let f = FileFormat::for_extension("sql").unwrap();
        assert_eq!(norm(f, "SELECT 1 -- why"), "SELECT 1");
        assert_eq!(norm(f, "-- note"), "");
    }

    #[test]
    fn semicolon_family() {
        let f = FileFormat::for_extension("ini").unwrap();
        assert_eq!(norm(f, "key=1 ; note"), "key=1");
        assert_eq!(norm(f, "; comment"), "");
    }

    #[test]
    fn batch_family() {
        let f = FileFormat::for_extension("bat").unwrap();
        assert_eq!(norm(f, "REM a comment"), "");
        assert_eq!(norm(f, "  rem lower case"), "");
        assert_eq!(norm(f, "rem"), "");
        assert_eq!(norm(f, ":: label comment"), "");
        assert_eq!(norm(f, "echo hi"), "echo hi");
        // `remove` is not REM
        assert_eq!(norm(f, "remove x"), "remove x");
    }

    #[test]
    fn markup_family() {
        let f = FileFormat::for_extension("xml").unwrap();
        assert_eq!(norm(f, "<a/> <!-- note -->"), "<a/>");
        assert_eq!(norm(f, "<!-- a --><b/><!-- c -->"), "<b/>");
    }

    #[test]
    fn documented_limitation_markers_in_strings_are_removed() {
        // D9: comment markers inside string literals are a known v1 limitation.
        let f = FileFormat::for_extension("c").unwrap();
        assert_eq!(norm(f, "url = \"http://example.com\";"), "url = \"http:");
    }
}
