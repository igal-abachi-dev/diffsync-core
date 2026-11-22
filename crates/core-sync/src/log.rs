//! The execution log (plan.md §8.3, backlog E8-S6): JSON lines, one per action, flushed after
//! every line so the file is complete even when the job is aborted or the process dies.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::exec::{JobResult, Step};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC) for a point in time.
pub fn iso_time(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (secs, millis) = (d.as_secs() as i64, d.subsec_millis());
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[derive(Debug)]
pub struct JobLog {
    file: std::fs::File,
    finished: bool,
}

impl JobLog {
    /// Creates `<dir>/<name>-<unix seconds>-<n>.jsonl` and writes the start line.
    pub fn create(dir: &Path, name: &str, steps: usize) -> std::io::Result<JobLog> {
        std::fs::create_dir_all(dir)?;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // `create_new` + a new sequence number on a clash: two processes (or two jobs of one
        // second) that share a log folder never overwrite each other's audit log
        let mut attempts = 0;
        let file = loop {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join(format!("{name}-{secs}-{n}.jsonl")))
            {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 1000 => {
                    attempts += 1;
                }
                other => break other?,
            }
        };
        let mut l = JobLog {
            file,
            finished: false,
        };
        l.line(&json!({"time": iso_time(SystemTime::now()), "event": "start", "job": name, "steps": steps}));
        Ok(l)
    }

    fn line(&mut self, v: &serde_json::Value) {
        // a log that cannot be written must not stop the job
        let _ = writeln!(self.file, "{v}");
        let _ = self.file.flush();
    }

    /// One line per action: timestamp, action, paths, bytes, outcome and error.
    pub fn action(&mut self, step: &Step, outcome: &str, note: Option<&str>, bytes: u64) {
        let v = json!({
            "time": iso_time(SystemTime::now()),
            "event": "action",
            "action": step.label,
            "src": step.src.as_ref().map(|s| s.path.display()),
            "dst": step.dst.path.display(),
            "bytes": bytes,
            "outcome": outcome,
            "error": note,
        });
        self.line(&v);
    }

    pub fn finish(&mut self, r: &JobResult) {
        let v = json!({
            "time": iso_time(SystemTime::now()),
            "event": "end",
            "done": r.done, "skipped": r.skipped, "failed": r.failed, "kept": r.kept,
            "bytesCopied": r.bytes_copied, "aborted": r.aborted, "cancelled": r.cancelled,
        });
        self.line(&v);
        self.finished = true;
    }
}

impl Drop for JobLog {
    fn drop(&mut self) {
        // a job that ended without `finish` (a panic) still leaves a closing line
        if !self.finished {
            let v =
                json!({"time": iso_time(SystemTime::now()), "event": "end", "terminated": true});
            self.line(&v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_time_is_correct_for_known_instants() {
        assert_eq!(iso_time(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_000_000_000_123);
        assert_eq!(iso_time(t), "2001-09-09T01:46:40.123Z");
        let leap = UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800); // 2024-02-29
        assert_eq!(iso_time(leap), "2024-02-29T00:00:00.000Z");
    }
}
