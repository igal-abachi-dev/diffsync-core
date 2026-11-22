//! JSON Patch, RFC 6902 (backlog E11.5-S16): the difference between two JSON documents as a list
//! of `add` / `remove` / `replace` operations, and applying such a list (also `move`, `copy`,
//! `test`).
//!
//! For configuration, manifests and API payloads a model reads `{"op": "replace", "path":
//! "/config/x", "value": 2}` better than a line diff of the pretty-printed file, and it does not
//! break when the formatting changes. Applying is atomic (RFC 6902 §5): the document is changed
//! only when every operation succeeded.
//!
//! The diff recurses into objects and, for arrays, drops the equal head and tail and compares the
//! middle element by element; the result is valid and exact, not always the shortest possible
//! list (decision D60).

use serde_json::{Map, Value, json};

/// The operations that turn `old` into `new`, as JSON objects.
pub fn diff(old: &Value, new: &Value) -> Vec<Value> {
    let mut ops = Vec::new();
    walk(old, new, "", &mut ops);
    ops
}

/// `~` and `/` in a key (RFC 6901).
pub fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn child(path: &str, token: &str) -> String {
    format!("{path}/{}", escape(token))
}

fn walk(old: &Value, new: &Value, path: &str, ops: &mut Vec<Value>) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            for k in a.keys().filter(|k| !b.contains_key(*k)) {
                ops.push(json!({"op": "remove", "path": child(path, k)}));
            }
            for (k, v) in a {
                if let Some(nv) = b.get(k) {
                    let p = child(path, k);
                    walk(v, nv, &p, ops);
                }
            }
            for (k, v) in b.iter().filter(|(k, _)| !a.contains_key(*k)) {
                ops.push(json!({"op": "add", "path": child(path, k), "value": v}));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
            let tail = a[head..]
                .iter()
                .rev()
                .zip(b[head..].iter().rev())
                .take_while(|(x, y)| x == y)
                .count();
            let (ma, mb) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
            let common = ma.len().min(mb.len());
            for k in 0..common {
                let p = child(path, &(head + k).to_string());
                walk(&ma[k], &mb[k], &p, ops);
            }
            // surplus old elements go from the back so the indexes stay valid
            for k in (common..ma.len()).rev() {
                ops.push(json!({"op": "remove", "path": child(path, &(head + k).to_string())}));
            }
            for (k, v) in mb.iter().enumerate().skip(common) {
                ops.push(
                    json!({"op": "add", "path": child(path, &(head + k).to_string()), "value": v}),
                );
            }
        }
        _ => ops.push(json!({"op": "replace", "path": path, "value": new})),
    }
}

/// An operation that could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    /// 0-based index in the patch.
    pub index: usize,
    pub message: String,
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation {}: {}", self.index, self.message)
    }
}

impl std::error::Error for OpError {}

fn tokens(ptr: &str) -> Result<Vec<String>, String> {
    if ptr.is_empty() {
        return Ok(Vec::new());
    }
    let rest = ptr
        .strip_prefix('/')
        .ok_or_else(|| format!("'{ptr}' is not a JSON pointer"))?;
    Ok(rest
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect())
}

fn index(tok: &str, len: usize, allow_end: bool) -> Result<usize, String> {
    if allow_end && tok == "-" {
        return Ok(len);
    }
    if tok.is_empty()
        || (tok.len() > 1 && tok.starts_with('0'))
        || !tok.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("'{tok}' is not an array index"));
    }
    let i: usize = tok
        .parse()
        .map_err(|_| format!("'{tok}' is out of range"))?;
    if i > len || (i == len && !allow_end) {
        return Err(format!("index {i} is beyond the array (length {len})"));
    }
    Ok(i)
}

fn get<'a>(doc: &'a Value, toks: &[String]) -> Result<&'a Value, String> {
    let mut cur = doc;
    for t in toks {
        cur = match cur {
            Value::Object(m) => m.get(t).ok_or_else(|| format!("no member '{t}'"))?,
            Value::Array(a) => &a[index(t, a.len(), false)?],
            _ => return Err(format!("cannot look into a scalar at '{t}'")),
        };
    }
    Ok(cur)
}

fn get_mut<'a>(doc: &'a mut Value, toks: &[String]) -> Result<&'a mut Value, String> {
    let mut cur = doc;
    for t in toks {
        cur = match cur {
            Value::Object(m) => m.get_mut(t).ok_or_else(|| format!("no member '{t}'"))?,
            Value::Array(a) => {
                let i = index(t, a.len(), false)?;
                &mut a[i]
            }
            _ => return Err(format!("cannot look into a scalar at '{t}'")),
        };
    }
    Ok(cur)
}

fn add(doc: &mut Value, ptr: &str, v: Value) -> Result<(), String> {
    let toks = tokens(ptr)?;
    let Some((last, parent)) = toks.split_last() else {
        *doc = v;
        return Ok(());
    };
    match get_mut(doc, parent)? {
        Value::Object(m) => {
            m.insert(last.clone(), v);
        }
        Value::Array(a) => {
            let i = index(last, a.len(), true)?;
            a.insert(i, v);
        }
        _ => return Err("the parent is not an object or array".into()),
    }
    Ok(())
}

fn remove(doc: &mut Value, ptr: &str) -> Result<Value, String> {
    let toks = tokens(ptr)?;
    let Some((last, parent)) = toks.split_last() else {
        return Err("the whole document cannot be removed".into());
    };
    match get_mut(doc, parent)? {
        Value::Object(m) => m
            .remove(last)
            .ok_or_else(|| format!("no member '{last}' to remove")),
        Value::Array(a) => {
            let i = index(last, a.len(), false)?;
            Ok(a.remove(i))
        }
        _ => Err("the parent is not an object or array".into()),
    }
}

/// Applies `patch` to a copy of `doc`; the result, or the first operation that failed.
pub fn apply(doc: &Value, patch: &[Value]) -> Result<Value, OpError> {
    let mut d = doc.clone();
    for (index, op) in patch.iter().enumerate() {
        let bad = |message: String| OpError { index, message };
        let obj: &Map<String, Value> = op
            .as_object()
            .ok_or_else(|| bad("an operation must be an object".into()))?;
        let name = obj
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("missing \"op\"".into()))?;
        let path = obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("missing \"path\"".into()))?;
        let value = || {
            obj.get("value")
                .cloned()
                .ok_or_else(|| bad("missing \"value\"".into()))
        };
        let from = || {
            obj.get("from")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("missing \"from\"".into()))
        };
        match name {
            "add" => add(&mut d, path, value()?).map_err(bad)?,
            "remove" => {
                remove(&mut d, path).map_err(bad)?;
            }
            "replace" => {
                let toks = tokens(path).map_err(bad)?;
                let target = get_mut(&mut d, &toks).map_err(bad)?;
                *target = value()?;
            }
            "move" => {
                let f = from()?;
                if path.starts_with(&format!("{f}/")) {
                    return Err(bad("cannot move a value into itself".into()));
                }
                let v = remove(&mut d, f).map_err(bad)?;
                add(&mut d, path, v).map_err(bad)?;
            }
            "copy" => {
                let v = get(&d, &tokens(from()?).map_err(bad)?)
                    .map_err(bad)?
                    .clone();
                add(&mut d, path, v).map_err(bad)?;
            }
            "test" => {
                let have = get(&d, &tokens(path).map_err(bad)?).map_err(bad)?;
                if *have != value()? {
                    return Err(bad(format!("test failed at '{path}'")));
                }
            }
            other => return Err(bad(format!("unknown operation '{other}'"))),
        }
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)]
    fn rt(a: Value, b: Value) -> Vec<Value> {
        let ops = diff(&a, &b);
        assert_eq!(apply(&a, &ops).unwrap(), b, "ops: {ops:?}");
        ops
    }

    #[test]
    fn the_example_of_the_request() {
        let ops = rt(json!({"config": {"x": 1}}), json!({"config": {"x": 2}}));
        assert_eq!(
            ops,
            vec![json!({"op": "replace", "path": "/config/x", "value": 2})]
        );
    }

    #[test]
    fn objects_arrays_and_escapes_round_trip() {
        rt(
            json!({"a": 1, "b": [1, 2, 3], "c/d": {"~": true}}),
            json!({"b": [1, 9, 2, 3], "c/d": {"~": false}, "e": null}),
        );
        rt(json!([1, 2, 3, 4, 5]), json!([1, 5]));
        rt(json!([1, 5]), json!([1, 2, 3, 4, 5]));
        rt(json!([{"id": 1}, {"id": 2}]), json!([{"id": 1, "n": "x"}]));
        rt(json!(1), json!({"now": "an object"}));
        let ops = rt(json!({"a/b": 1}), json!({"a/b": 2}));
        assert_eq!(ops[0]["path"], "/a~1b");
        assert!(diff(&json!({"a": [1]}), &json!({"a": [1]})).is_empty());
        // a single insertion in a long array is one operation
        let mut long: Vec<Value> = (0..50).map(|i| json!(i)).collect();
        let a = Value::Array(long.clone());
        long.insert(20, json!("new"));
        assert_eq!(rt(a, Value::Array(long)).len(), 1);
    }

    #[test]
    fn apply_handles_move_copy_test_and_is_atomic() {
        let d = json!({"a": {"x": 1}, "l": [1, 2]});
        let p: Vec<Value> = serde_json::from_str(
            r#"[{"op":"test","path":"/a/x","value":1},
                {"op":"copy","from":"/a/x","path":"/l/-"},
                {"op":"move","from":"/a","path":"/b"},
                {"op":"add","path":"/l/0","value":0}]"#,
        )
        .unwrap();
        assert_eq!(
            apply(&d, &p).unwrap(),
            json!({"b": {"x": 1}, "l": [0, 1, 2, 1]})
        );
        let bad: Vec<Value> =
            serde_json::from_str(r#"[{"op":"remove","path":"/a"},{"op":"remove","path":"/nope"}]"#)
                .unwrap();
        let e = apply(&d, &bad).unwrap_err();
        assert_eq!(e.index, 1);
        let failing_test: Vec<Value> =
            serde_json::from_str(r#"[{"op":"test","path":"/a/x","value":2}]"#).unwrap();
        assert!(
            apply(&d, &failing_test)
                .unwrap_err()
                .message
                .contains("test failed")
        );
        for text in [
            r#"[{"op":"add","path":"/l/05","value":1}]"#,
            r#"[{"op":"add","path":"/l/9","value":1}]"#,
            r#"[{"op":"frob","path":"/a"}]"#,
            r#"[{"op":"remove","path":""}]"#,
            r#"[{"op":"add","path":"a","value":1}]"#,
        ] {
            let p: Vec<Value> = serde_json::from_str(text).unwrap();
            assert!(apply(&d, &p).is_err(), "{text}");
        }
    }
}
