//! Text compare options: the JSON shape used by fixtures, `dsdev` and (later) the C ABI
//! (plan.md §5.4, §10.1). Only the keys that differ from the defaults need to be written.
//!
//! ```json
//! { "alignment": "standard", "skewTolerance": 2000, "useCloseness": true,
//!   "closenessSameOnly": false, "neverAlignDiffs": false, "orphansAlwaysSig": false,
//!   "ignoreCase": false, "whitespace": "leadingTrailing", "unimportant": ["//.*$"] }
//! ```

use core_base::{Error, Result};
use core_text::rules::{LineRules, WhitespaceMode};
use serde::{Deserialize, Serialize};

use crate::align::{AlignConfig, AlignKind};

/// Alignment kind names in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlignmentName {
    None,
    Patience,
    Myers,
    #[default]
    Standard,
    Histogram,
}

/// Whitespace mode names in JSON (the names the reference model uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WhitespaceName {
    #[serde(rename = "all")]
    All,
    #[default]
    #[serde(rename = "leadingTrailing")]
    LeadingTrailing,
    #[serde(rename = "changes")]
    Changes,
    #[serde(rename = "ignoreAll")]
    IgnoreAll,
}

/// How line endings are compared, in JSON (decision: D55).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EolName {
    /// Not compared: a CRLF file and an LF file are equal (the visual compare).
    #[default]
    #[serde(rename = "ignore")]
    Ignore,
    /// CRLF, LF, CR and a missing final ending all differ (`diff`, `git diff`).
    #[serde(rename = "strict")]
    Strict,
    /// CRLF and LF are the same, a missing final ending is not (`--strip-trailing-cr`).
    #[serde(rename = "crlfAsLf")]
    CrLfAsLf,
}

/// Everything that decides how two texts are aligned and how their lines are compared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TextOptions {
    pub alignment: AlignmentName,
    pub skew_tolerance: u32,
    /// Absent = the default of the alignment kind (on, except for Myers).
    pub use_closeness: Option<bool>,
    pub closeness_same_only: bool,
    pub never_align_diffs: bool,
    pub orphans_always_sig: bool,
    pub ignore_case: bool,
    pub whitespace: WhitespaceName,
    /// Regexes whose matches are unimportant text.
    pub unimportant: Vec<String>,
    /// Whether line endings take part in the comparison.
    pub eol: EolName,
}

impl Default for TextOptions {
    fn default() -> Self {
        TextOptions {
            alignment: AlignmentName::Standard,
            skew_tolerance: crate::align::consts::DEFAULT_SKEW,
            use_closeness: None,
            closeness_same_only: false,
            never_align_diffs: false,
            orphans_always_sig: false,
            ignore_case: false,
            whitespace: WhitespaceName::LeadingTrailing,
            unimportant: Vec::new(),
            eol: EolName::Ignore,
        }
    }
}

impl TextOptions {
    /// Parses options JSON; unknown keys are ignored, a wrong value is `InvalidArgument`.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| Error::invalid_argument(format!("bad options: {e}")))
    }

    /// The aligner settings. Like the original's `TDcTextComparison`, `never_align_diffs`
    /// switches closeness off and a skew below 1 is raised to 1.
    pub fn align_config(&self) -> AlignConfig {
        AlignConfig {
            kind: match self.alignment {
                AlignmentName::None => AlignKind::None,
                AlignmentName::Patience => AlignKind::Patience,
                AlignmentName::Myers => AlignKind::Myers,
                AlignmentName::Standard => AlignKind::Standard,
                AlignmentName::Histogram => AlignKind::Histogram,
            },
            skew_tolerance: self.skew_tolerance.max(1),
            use_closeness: if self.never_align_diffs {
                Some(false)
            } else {
                self.use_closeness
            },
            closeness_same_only: self.closeness_same_only,
            never_align_diffs: self.never_align_diffs,
            orphans_always_sig: self.orphans_always_sig,
        }
    }

    /// The line rules; an invalid regex is `InvalidArgument`.
    pub fn line_rules(&self) -> Result<LineRules> {
        LineRules::new()
            .with_ignore_case(self.ignore_case)
            .with_whitespace(match self.whitespace {
                WhitespaceName::All => WhitespaceMode::CompareAll,
                WhitespaceName::LeadingTrailing => WhitespaceMode::IgnoreLeadingTrailing,
                WhitespaceName::Changes => WhitespaceMode::IgnoreChanges,
                WhitespaceName::IgnoreAll => WhitespaceMode::IgnoreAll,
            })
            .with_eol(match self.eol {
                EolName::Ignore => core_text::EolCompare::Ignore,
                EolName::Strict => core_text::EolCompare::Strict,
                EolName::CrLfAsLf => core_text::EolCompare::CrLfAsLf,
            })
            .with_unimportant(&self.unimportant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_object_gives_the_defaults() {
        let o = TextOptions::from_json("{}").unwrap();
        assert_eq!(o, TextOptions::default());
        let c = o.align_config();
        assert_eq!(c, AlignConfig::default());
        assert_eq!(
            o.line_rules().unwrap().whitespace(),
            WhitespaceMode::IgnoreLeadingTrailing
        );
    }

    #[test]
    fn every_key_is_read() {
        let o = TextOptions::from_json(
            r##"{"alignment":"histogram","skewTolerance":7,"useCloseness":false,
                "closenessSameOnly":true,"orphansAlwaysSig":true,"ignoreCase":true,
                "whitespace":"changes","unimportant":["#.*$"]}"##,
        )
        .unwrap();
        let c = o.align_config();
        assert_eq!(c.kind, AlignKind::Histogram);
        assert_eq!(c.skew_tolerance, 7);
        assert_eq!(c.use_closeness, Some(false));
        assert!(c.closeness_same_only && c.orphans_always_sig);
        let r = o.line_rules().unwrap();
        assert!(r.ignore_case());
        assert_eq!(r.whitespace(), WhitespaceMode::IgnoreChanges);
        assert_eq!(r.unimportant_patterns().collect::<Vec<_>>(), ["#.*$"]);
    }

    #[test]
    fn never_align_diffs_turns_closeness_off_and_zero_skew_becomes_one() {
        let o = TextOptions::from_json(r#"{"neverAlignDiffs":true,"skewTolerance":0}"#).unwrap();
        let c = o.align_config();
        assert_eq!(c.use_closeness, Some(false));
        assert_eq!(c.skew_tolerance, 1);
        assert!(c.resolve().is_ok());
    }

    #[test]
    fn unknown_keys_are_ignored_and_bad_values_rejected() {
        assert!(TextOptions::from_json(r#"{"version":1,"futureKey":3}"#).is_ok());
        assert!(TextOptions::from_json(r#"{"alignment":"quantum"}"#).is_err());
        assert!(TextOptions::from_json(r#"{"whitespace":"bogus"}"#).is_err());
        assert!(TextOptions::from_json("not json").is_err());
        let bad = TextOptions::from_json(r#"{"unimportant":["(x"]}"#).unwrap();
        assert!(bad.line_rules().is_err());
    }

    #[test]
    fn serializes_back_to_the_same_keys() {
        let json = serde_json::to_string(&TextOptions::default()).unwrap();
        assert!(json.contains("\"skewTolerance\":2000"), "{json}");
        assert_eq!(
            TextOptions::from_json(&json).unwrap(),
            TextOptions::default()
        );
    }
}
