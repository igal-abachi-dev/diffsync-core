//! Options JSON and the schema export (plan.md §10.1; backlog E9-S3). Every option set has
//! `"version": 1`, defaults for missing fields, and unknown fields as warnings.
//!
//! Decision D45: the schema is generated from the serialized defaults of the option structs
//! (field names, JSON types, default values) plus a table of enum values, instead of deriving
//! `schemars` on structs in four crates. A test checks that every field of every default is in
//! the schema and that sample options validate.

use std::path::PathBuf;

use core_diff::TextOptions;
use core_folder::{FolderRules, SortKey, SortOrder, ViewFilter};
use core_sync::{ExecOptions, ExecOverrides, FileOpOptions, Preset, SyncRules};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::events::log;
use crate::status::{Fail, Res};

/// Reports unknown fields of an options object as warnings (to the log sink) and checks the
/// version.
fn check_object(json: &str, known: &[&str], what: &str) -> Res<Value> {
    let v: Value = if json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(json).map_err(|e| Fail::invalid(format!("bad {what} options: {e}")))?
    };
    let Some(o) = v.as_object() else {
        return Err(Fail::invalid(format!(
            "{what} options must be a JSON object"
        )));
    };
    if let Some(ver) = o.get("version") {
        if ver.as_u64() != Some(1) {
            return Err(Fail::invalid(format!(
                "unsupported {what} options version {ver}"
            )));
        }
    }
    for k in o.keys() {
        if k != "version" && !known.contains(&k.as_str()) {
            log(1, &format!("unknown {what} option `{k}` ignored"));
        }
    }
    Ok(v)
}

fn keys_of<T: serde::Serialize + Default>() -> Vec<String> {
    serde_json::to_value(T::default())
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default()
}

pub fn text_options(json: &str) -> Res<TextOptions> {
    let known = keys_of::<TextOptions>();
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let v = check_object(json, &known, "text")?;
    serde_json::from_value(v).map_err(|e| Fail::invalid(format!("bad text options: {e}")))
}

pub fn folder_rules(json: &str) -> Res<FolderRules> {
    let known = keys_of::<FolderRules>();
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let v = check_object(json, &known, "folder")?;
    serde_json::from_value(v).map_err(|e| Fail::invalid(format!("bad folder options: {e}")))
}

pub fn sync_rules(json: &str) -> Res<SyncRules> {
    let known = keys_of::<SyncRules>();
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let v = check_object(json, &known, "sync rules")?;
    serde_json::from_value(v).map_err(|e| Fail::invalid(format!("bad sync rules: {e}")))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ExecJson {
    log_dir: Option<String>,
    job_name: Option<String>,
    verify: Option<bool>,
    use_recycle_bin: Option<bool>,
    /// Flush copies to the storage device before they replace their destination; default true.
    durable_copy: Option<bool>,
    /// A log that was asked for must work, or the job does not run (default true).
    log_required: Option<bool>,
    overwrite: bool,
    /// Tries again after a transient copy failure (robocopy's `/R`); default 2.
    copy_retries: Option<u32>,
    /// Milliseconds between the tries (robocopy's `/W`); default 1000.
    copy_retry_wait_ms: Option<u32>,
}

const EXEC_KEYS: &[&str] = &[
    "logDir",
    "jobName",
    "verify",
    "useRecycleBin",
    "durableCopy",
    "logRequired",
    "overwrite",
    "copyRetries",
    "copyRetryWaitMs",
];

fn parse_exec(json: &str) -> Res<ExecJson> {
    let v = check_object(json, EXEC_KEYS, "exec")?;
    serde_json::from_value(v).map_err(|e| Fail::invalid(format!("bad exec options: {e}")))
}

fn default_log_dir() -> Option<PathBuf> {
    crate::DEFAULT_LOG_DIR.lock().clone()
}

/// A log folder from JSON: empty means "none given"; a relative path is refused, because it would
/// resolve against the process current directory (AGENTS.md).
fn log_dir_of(s: Option<&str>) -> Res<Option<PathBuf>> {
    match s {
        None | Some("") => Ok(default_log_dir()),
        Some(d) if !PathBuf::from(d).is_absolute() => {
            Err(Fail::invalid("logDir must be an absolute path"))
        }
        Some(d) => Ok(Some(PathBuf::from(d))),
    }
}

/// What a caller sets when it executes a sync plan (decision: D46): the log, and optional
/// `verify` / `useRecycleBin` on top of the plan's own rules.
pub fn exec_overrides(json: &str) -> Res<ExecOverrides> {
    let e = parse_exec(json)?;
    Ok(ExecOverrides {
        log_dir: log_dir_of(e.log_dir.as_deref())?,
        job_name: job_name_of(e.job_name)?,
        verify: e.verify,
        use_recycle_bin: e.use_recycle_bin,
        durable_copy: e.durable_copy,
        log_required: e.log_required,
        copy_retries: e.copy_retries,
        copy_retry_wait_ms: e.copy_retry_wait_ms,
    })
}

/// The job name becomes part of the log file name, so it must stay a plain name: a separator, a
/// drive colon or `..` would move the log out of `logDir` (reviewer, E11-S6).
fn job_name_of(n: Option<String>) -> Res<Option<String>> {
    match n.filter(|n| !n.is_empty()) {
        None => Ok(None),
        Some(n)
            if n.chars().any(|c| matches!(c, '/' | '\\' | ':' | '\0'))
                || n.contains("..")
                || n.len() > 100 =>
        {
            Err(Fail::invalid(
                "jobName must be a plain name: no path separators, ':' or '..', at most 100 bytes",
            ))
        }
        Some(n) => Ok(Some(n)),
    }
}

pub fn fileop_options(json: &str) -> Res<FileOpOptions> {
    let e = parse_exec(json)?;
    let mut exec = ExecOptions {
        log_dir: log_dir_of(e.log_dir.as_deref())?,
        ..ExecOptions::default()
    };
    if let Some(n) = job_name_of(e.job_name)? {
        exec.job_name = n;
    }
    if let Some(v) = e.verify {
        exec.verify = v;
    }
    if let Some(v) = e.use_recycle_bin {
        exec.use_recycle_bin = v;
    }
    if let Some(v) = e.durable_copy {
        exec.durable_copy = v;
    }
    if let Some(v) = e.log_required {
        exec.log_required = v;
    }
    if let Some(v) = e.copy_retries {
        exec.copy_retries = v;
    }
    if let Some(v) = e.copy_retry_wait_ms {
        exec.copy_retry_wait_ms = v;
    }
    Ok(FileOpOptions {
        overwrite: e.overwrite,
        exec,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewJson {
    pub filter: Option<String>,
    pub search: Option<String>,
    pub sort: Option<SortJson>,
    /// Treat differences that are only unimportant as equal.
    pub ignore_unimportant: Option<bool>,
    /// Show items marked as ignored.
    pub show_ignored: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SortJson {
    pub key: Option<String>,
    pub descending: bool,
}

pub fn view_json(json: &str) -> Res<ViewJson> {
    let v = check_object(
        json,
        &[
            "filter",
            "search",
            "sort",
            "ignoreUnimportant",
            "showIgnored",
        ],
        "view",
    )?;
    serde_json::from_value(v).map_err(|e| Fail::invalid(format!("bad view options: {e}")))
}

pub fn filter_of(s: &str) -> Res<ViewFilter> {
    Ok(match s {
        "all" => ViewFilter::All,
        "differences" => ViewFilter::Differences,
        "same" => ViewFilter::Same,
        "orphans" => ViewFilter::Orphans,
        "leftNewer" => ViewFilter::LeftNewer,
        "rightNewer" => ViewFilter::RightNewer,
        "noOrphans" => ViewFilter::NoOrphans,
        "differencesNoOrphans" => ViewFilter::DifferencesNoOrphans,
        "leftOrphans" => ViewFilter::LeftOrphans,
        "rightOrphans" => ViewFilter::RightOrphans,
        "leftNewerOrphans" => ViewFilter::LeftNewerOrphans,
        "rightNewerOrphans" => ViewFilter::RightNewerOrphans,
        other => return Err(Fail::invalid(format!("unknown view filter `{other}`"))),
    })
}

pub fn sort_of(s: &SortJson) -> Res<SortOrder> {
    let key = match s.key.as_deref().unwrap_or("name") {
        "name" => SortKey::Name,
        "size" => SortKey::Size,
        "time" => SortKey::Time,
        "status" => SortKey::Status,
        other => return Err(Fail::invalid(format!("unknown sort key `{other}`"))),
    };
    Ok(SortOrder {
        key,
        descending: s.descending,
    })
}

// ---------------------------------------------------------------- schema

/// Enum-valued fields: `(section, field, values)`.
const ENUMS: &[(&str, &str, &[&str])] = &[
    (
        "text",
        "alignment",
        &["standard", "none", "patience", "myers", "histogram"],
    ),
    (
        "text",
        "whitespace",
        &["all", "leadingTrailing", "changes", "ignoreAll"],
    ),
    ("text", "eol", &["ignore", "strict", "crlfAsLf"]),
    ("folder", "contentKind", &["crc32", "binary", "rulesBased"]),
    (
        "sync",
        "preset",
        &[
            "updateRight",
            "updateLeft",
            "updateBoth",
            "mirrorToRight",
            "mirrorToLeft",
        ],
    ),
    (
        "view",
        "filter",
        &[
            "all",
            "differences",
            "same",
            "orphans",
            "leftNewer",
            "rightNewer",
            "noOrphans",
            "differencesNoOrphans",
            "leftOrphans",
            "rightOrphans",
            "leftNewerOrphans",
            "rightNewerOrphans",
        ],
    ),
];

fn schema_of(section: &str, default: &Value) -> Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "version".into(),
        json!({"type": "integer", "const": 1, "default": 1}),
    );
    if let Some(o) = default.as_object() {
        for (k, v) in o {
            let mut p = serde_json::Map::new();
            let ty = match v {
                Value::Bool(_) => "boolean",
                Value::Number(n) if n.is_f64() => "number",
                Value::Number(_) => "integer",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
                Value::Null => "null",
            };
            p.insert("type".into(), json!(ty));
            p.insert("default".into(), v.clone());
            if let Some((_, _, vals)) = ENUMS.iter().find(|(s, f, _)| *s == section && f == k) {
                p.insert("enum".into(), json!(vals));
            }
            if ty == "array" {
                p.insert("items".into(), json!({"type": "string"}));
            }
            if ty == "object" {
                // nested option objects (the text options inside folder rules)
                p.insert("additionalProperties".into(), json!(true));
            }
            props.insert(k.clone(), Value::Object(p));
        }
    }
    json!({"type": "object", "properties": props, "additionalProperties": true})
}

/// The JSON Schema of every option set (draft 2020-12), as UTF-8 JSON.
pub fn schema_json() -> String {
    let to_v = |v: Value| v;
    let text = to_v(serde_json::to_value(TextOptions::default()).unwrap_or_default());
    let folder = to_v(serde_json::to_value(FolderRules::default()).unwrap_or_default());
    let sync = to_v(serde_json::to_value(SyncRules::default()).unwrap_or_default());
    let exec = json!({
        "logDir": "", "jobName": "", "verify": false, "useRecycleBin": false, "overwrite": false,
        "durableCopy": true, "logRequired": true, "copyRetries": 2, "copyRetryWaitMs": 1000
    });
    let view = json!({"filter": "all", "search": "", "sort": {}});
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "diffsync options",
        "description": "Every option set takes \"version\": 1, defaults for missing fields; unknown fields are reported as warnings and ignored.",
        "abiOptionsVersion": 1,
        "properties": {
            "text": schema_of("text", &text),
            "folder": schema_of("folder", &folder),
            "sync": schema_of("sync", &sync),
            "exec": schema_of("exec", &exec),
            "view": schema_of("view", &view),
        }
    });
    schema.to_string()
}

/// The presets and statuses a UI form needs, kept next to the schema for tests.
pub const PRESET_NAMES: [(&str, Preset); 5] = [
    ("updateRight", Preset::UpdateRight),
    ("updateLeft", Preset::UpdateLeft),
    ("updateBoth", Preset::UpdateBoth),
    ("mirrorToRight", Preset::MirrorToRight),
    ("mirrorToLeft", Preset::MirrorToLeft),
];
