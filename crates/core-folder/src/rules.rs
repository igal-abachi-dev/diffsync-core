//! Folder compare rules and `times_equal` (plan.md §7.1, §7.3 step 6, decision D7; backlog E7-S1,
//! E7-S6.1). Defaults come from FOLDER-2.

use core_base::ticks::TICKS_PER_SECOND;
use core_base::{Error, Result, Ticks};
use core_diff::TextOptions;
use serde::{Deserialize, Serialize};

/// How file content is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentKind {
    /// Standard CRC-32 of each file.
    Crc32,
    /// Byte for byte, stopping at the first difference.
    Binary,
    /// Text compare in verdict mode (whitespace, endings, comments can be unimportant).
    #[default]
    RulesBased,
}

/// The rules of a folder compare. JSON keys are camelCase; missing keys take the defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderRules {
    pub compare_size: bool,
    pub compare_timestamp: bool,
    pub timestamp_tolerance_s: u32,
    /// A difference of exactly 1 hour (± tolerance) counts as equal.
    pub ignore_dst: bool,
    /// Differences that are a multiple of 15 minutes up to 14 hours count as equal (D7).
    pub ignore_time_zone: bool,
    pub compare_attributes: bool,
    pub case_sensitive_names: bool,
    /// Pair names that differ only in Unicode normalisation (NFC).
    pub normalized_alignment: bool,
    /// Pair `a.txt` with `a.bak` (files only).
    pub align_ignore_extensions: bool,
    pub compare_content: bool,
    pub content_kind: ContentKind,
    /// The content result overrides the size/time result (same content = same, whatever the times).
    pub content_overrides: bool,
    /// Skip the content test when size and time already say "same".
    pub content_skip_same: bool,
    /// Reserved: links are never followed in v1 (decision D31); the flag is kept for the ABI.
    pub follow_symlinks: bool,
    /// Content compare worker limit (default 4; decision D30 replaces `parallel_content`).
    pub content_workers: usize,
    pub include_files: Vec<String>,
    pub exclude_files: Vec<String>,
    pub exclude_folders: Vec<String>,
    pub include_hidden: bool,
    pub include_system: bool,
    /// Allow content compare of cloud placeholders (it downloads them; D17).
    pub compare_offline_files: bool,
    pub text: TextOptions,
}

impl Default for FolderRules {
    fn default() -> Self {
        FolderRules {
            compare_size: true,
            compare_timestamp: true,
            timestamp_tolerance_s: 2,
            ignore_dst: false,
            ignore_time_zone: false,
            compare_attributes: false,
            case_sensitive_names: false,
            normalized_alignment: true,
            align_ignore_extensions: false,
            compare_content: false,
            content_kind: ContentKind::RulesBased,
            content_overrides: true,
            content_skip_same: false,
            follow_symlinks: false,
            content_workers: 4,
            include_files: Vec::new(),
            exclude_files: Vec::new(),
            exclude_folders: Vec::new(),
            include_hidden: true,
            include_system: true,
            compare_offline_files: false,
            text: TextOptions::default(),
        }
    }
}

const KNOWN_KEYS: &[&str] = &[
    "version",
    "compareSize",
    "compareTimestamp",
    "timestampToleranceS",
    "ignoreDst",
    "ignoreTimeZone",
    "compareAttributes",
    "caseSensitiveNames",
    "normalizedAlignment",
    "alignIgnoreExtensions",
    "compareContent",
    "contentKind",
    "contentOverrides",
    "contentSkipSame",
    "followSymlinks",
    "contentWorkers",
    "includeFiles",
    "excludeFiles",
    "excludeFolders",
    "includeHidden",
    "includeSystem",
    "compareOfflineFiles",
    "text",
];

impl FolderRules {
    /// Parses rules JSON (`"version": 1`, missing keys default). Unknown keys are returned as
    /// warnings, never as errors (forward compatibility).
    pub fn from_json(json: &str) -> Result<(FolderRules, Vec<String>)> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| Error::invalid_argument(format!("bad rules: {e}")))?;
        let warnings: Vec<String> = v
            .as_object()
            .map(|o| {
                o.keys()
                    .filter(|k| !KNOWN_KEYS.contains(&k.as_str()))
                    .map(|k| format!("unknown rules field `{k}` ignored"))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(ver) = v.get("version").and_then(serde_json::Value::as_u64) {
            if ver != 1 {
                return Err(Error::invalid_argument(format!(
                    "unsupported rules version {ver}"
                )));
            }
        }
        let rules: FolderRules = serde_json::from_value(v)
            .map_err(|e| Error::invalid_argument(format!("bad rules: {e}")))?;
        Ok((rules, warnings))
    }

    pub fn to_json(&self) -> String {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("version".into(), 1.into());
        }
        v.to_string()
    }

    /// Whether two modification times count as equal under the rules (§7.3 step 6).
    pub fn times_equal(&self, left: Ticks, right: Ticks) -> bool {
        let delta = (i128::from(left) - i128::from(right)).abs();
        let tol = i128::from(self.timestamp_tolerance_s) * i128::from(TICKS_PER_SECOND);
        let hour = 3600 * i128::from(TICKS_PER_SECOND);
        if delta <= tol {
            return true;
        }
        if self.ignore_dst && (delta - hour).abs() <= tol {
            return true;
        }
        if self.ignore_time_zone {
            // decision: D7: every quarter-hour offset up to 14 h (+5:30 India, +5:45 Nepal, +12:45 Chatham)
            let quarter = 900 * i128::from(TICKS_PER_SECOND);
            return (1..=56).any(|k| (delta - k * quarter).abs() <= tol);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = TICKS_PER_SECOND;

    fn ticks(secs: f64) -> i64 {
        (secs * S as f64) as i64
    }

    /// The folder golden rows for times (plan.md §11.3, tolerance 2 s).
    #[test]
    fn tolerance_rows() {
        let r = FolderRules::default();
        assert!(r.times_equal(0, ticks(1.9)));
        assert!(r.times_equal(0, ticks(2.0)));
        assert!(!r.times_equal(0, ticks(2.1)));
        assert!(
            r.times_equal(ticks(2.1), 0) == r.times_equal(0, ticks(2.1)),
            "symmetric"
        );
    }

    #[test]
    fn dst_row() {
        let mut r = FolderRules::default();
        assert!(
            !r.times_equal(0, 3600 * S + 2 * S - 1 + S),
            "off: an hour apart is different"
        );
        assert!(!r.times_equal(0, 3600 * S));
        r.ignore_dst = true;
        assert!(r.times_equal(0, 3600 * S));
        assert!(r.times_equal(0, 3600 * S + 2 * S));
        assert!(r.times_equal(0, 3600 * S - 2 * S));
        assert!(!r.times_equal(0, 3600 * S + 3 * S));
    }

    #[test]
    fn time_zone_rows_including_india() {
        let mut r = FolderRules {
            ignore_time_zone: true,
            ..FolderRules::default()
        };
        assert!(r.times_equal(0, 19_800 * S), "5:30");
        assert!(
            !r.times_equal(0, 19_800 * S + 3 * S),
            "5:30 + 3 s is outside the tolerance"
        );
        assert!(r.times_equal(0, 20_700 * S), "5:45 Nepal");
        assert!(r.times_equal(0, 45_900 * S), "12:45 Chatham");
        assert!(r.times_equal(0, 14 * 3600 * S), "14 h");
        assert!(!r.times_equal(0, 14 * 3600 * S + 900 * S), "beyond 14 h");
        r.ignore_time_zone = false;
        assert!(!r.times_equal(0, 19_800 * S));
    }

    #[test]
    fn defaults_match_folder_2() {
        let d = FolderRules::default();
        assert!(
            d.compare_size && d.compare_timestamp && !d.compare_content && !d.case_sensitive_names
        );
        assert_eq!(
            (d.timestamp_tolerance_s, d.content_kind),
            (2, ContentKind::RulesBased)
        );
        assert!(d.normalized_alignment && d.content_overrides && !d.content_skip_same);
        assert!(
            d.include_hidden && d.include_system && !d.compare_offline_files && !d.follow_symlinks
        );
    }

    #[test]
    fn json_round_trip_defaults_and_unknown_fields() {
        let (r, w) = FolderRules::from_json("{}").unwrap();
        assert_eq!(r, FolderRules::default());
        assert!(w.is_empty());
        let (r, w) = FolderRules::from_json(r#"{"version":1,"timestampToleranceS":5,"contentKind":"crc32","futureThing":1,"excludeFiles":["*.obj"]}"#).unwrap();
        assert_eq!(
            (r.timestamp_tolerance_s, r.content_kind),
            (5, ContentKind::Crc32)
        );
        assert_eq!(r.exclude_files, ["*.obj"]);
        assert_eq!(w, ["unknown rules field `futureThing` ignored"]);
        let again = FolderRules::from_json(&r.to_json()).unwrap().0;
        assert_eq!(again, r);
        assert!(FolderRules::from_json(r#"{"version":2}"#).is_err());
        assert!(FolderRules::from_json("not json").is_err());
        assert!(FolderRules::from_json(r#"{"contentKind":"nope"}"#).is_err());
    }
}
