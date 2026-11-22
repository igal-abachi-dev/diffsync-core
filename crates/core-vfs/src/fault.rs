//! `FaultVfs<V>`: wraps a backend and injects failures for the safety tests (plan.md §6.2,
//! backlog E5-S11.2): fail the Nth operation of a kind, truncate or fail writes after N bytes,
//! cancel mid-copy, delay, and change a file between plan and execute.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use core_base::{CancelToken, Error, ErrorKind, Result, Ticks};
use parking_lot::Mutex;

use crate::path::VPath;
use crate::vfs::{CopyOptions, DeleteMode, Entry, Progress, ReadSeek, Vfs, VfsCaps};

/// The operations that can be counted and failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Stat,
    List,
    OpenRead,
    CreateTemp,
    CopyFile,
    Rename,
    Delete,
    CreateDir,
    SetTimes,
    SetAttrs,
}

/// A hook that runs when an operation happens (e.g. to modify the file system behind the
/// caller's back).
pub type Hook = Arc<dyn Fn(&VPath) + Send + Sync>;

#[derive(Default)]
struct Plan {
    counts: HashMap<Op, u64>,
    /// `(op, n)` fails the n-th call (1-based) with the error kind.
    fail_nth: Vec<(Op, u64, ErrorKind)>,
    /// Writes fail with `DiskFull` once this many bytes were written in total.
    fail_writes_after: Option<u64>,
    /// Cancel this token once this many bytes were written.
    cancel_after: Option<(u64, CancelToken)>,
    written: u64,
    delay: Option<Duration>,
    on_op: Vec<(Op, u64, Hook)>,
}

/// A fault-injecting wrapper.
#[allow(missing_debug_implementations)] // holds closures
pub struct FaultVfs<V: Vfs> {
    inner: V,
    plan: Arc<Mutex<Plan>>,
}

impl<V: Vfs> FaultVfs<V> {
    pub fn new(inner: V) -> Self {
        FaultVfs {
            inner,
            plan: Arc::new(Mutex::new(Plan::default())),
        }
    }

    pub fn inner(&self) -> &V {
        &self.inner
    }

    /// Fails the `n`-th (1-based) call of `op` with `kind`.
    pub fn fail_nth(&self, op: Op, n: u64, kind: ErrorKind) -> &Self {
        self.plan.lock().fail_nth.push((op, n, kind));
        self
    }

    /// Makes writes fail with `DiskFull` after `bytes` bytes in total (a full disk).
    pub fn fail_writes_after(&self, bytes: u64) -> &Self {
        self.plan.lock().fail_writes_after = Some(bytes);
        self
    }

    /// Cancels `token` once `bytes` bytes were written (cancel at 50% of a copy).
    pub fn cancel_after_bytes(&self, bytes: u64, token: &CancelToken) -> &Self {
        self.plan.lock().cancel_after = Some((bytes, token.clone()));
        self
    }

    pub fn delay_each_op(&self, d: Duration) -> &Self {
        self.plan.lock().delay = Some(d);
        self
    }

    /// Runs `hook` just before the `n`-th (1-based) call of `op`: change a file between the
    /// plan and the execute, for the `ChangedSinceCompare` tests.
    pub fn on_nth(&self, op: Op, n: u64, hook: Hook) -> &Self {
        self.plan.lock().on_op.push((op, n, hook));
        self
    }

    /// How often `op` was called.
    pub fn count(&self, op: Op) -> u64 {
        self.plan.lock().counts.get(&op).copied().unwrap_or(0)
    }

    pub fn total_bytes_written(&self) -> u64 {
        self.plan.lock().written
    }

    fn enter(&self, op: Op, p: &VPath) -> Result<()> {
        let (delay, hooks, fail) = {
            let mut plan = self.plan.lock();
            let n = plan.counts.entry(op).or_insert(0);
            *n += 1;
            let n = *n;
            let fail = plan
                .fail_nth
                .iter()
                .find(|(o, k, _)| *o == op && *k == n)
                .map(|f| f.2);
            let hooks: Vec<Hook> = plan
                .on_op
                .iter()
                .filter(|(o, k, _)| *o == op && *k == n)
                .map(|h| h.2.clone())
                .collect();
            (plan.delay, hooks, fail)
        };
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
        for h in hooks {
            h(p);
        }
        match fail {
            Some(kind) => {
                Err(Error::new(kind, format!("injected failure of {op:?}")).with_path(p.display()))
            }
            None => Ok(()),
        }
    }
}

struct FaultWriter {
    inner: Box<dyn Write + Send>,
    plan: Arc<Mutex<Plan>>,
}

impl Write for FaultWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (allow, cancel) = {
            let mut plan = self.plan.lock();
            if let Some(limit) = plan.fail_writes_after {
                if plan.written >= limit {
                    return Err(std::io::Error::other("injected: disk full"));
                }
            }
            plan.written += buf.len() as u64;
            let cancel = plan
                .cancel_after
                .as_ref()
                .filter(|(n, _)| plan.written >= *n)
                .map(|(_, t)| t.clone());
            (buf.len(), cancel)
        };
        if let Some(t) = cancel {
            t.cancel();
        }
        self.inner.write_all(&buf[..allow])?;
        Ok(allow)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<V: Vfs> Vfs for FaultVfs<V> {
    fn caps(&self) -> VfsCaps {
        self.inner.caps()
    }

    fn stat(&self, p: &VPath) -> Result<Option<Entry>> {
        self.enter(Op::Stat, p)?;
        self.inner.stat(p)
    }

    fn list(&self, dir: &VPath, out: &mut Vec<Entry>) -> Result<()> {
        self.enter(Op::List, dir)?;
        self.inner.list(dir, out)
    }

    fn open_read(&self, p: &VPath) -> Result<Box<dyn ReadSeek>> {
        self.enter(Op::OpenRead, p)?;
        self.inner.open_read(p)
    }

    fn create_temp_sibling(&self, p: &VPath) -> Result<(VPath, Box<dyn Write + Send>)> {
        self.enter(Op::CreateTemp, p)?;
        let (path, w) = self.inner.create_temp_sibling(p)?;
        Ok((
            path,
            Box::new(FaultWriter {
                inner: w,
                plan: self.plan.clone(),
            }),
        ))
    }

    fn copy_file(
        &self,
        src: &VPath,
        dst: &VPath,
        o: &CopyOptions,
        prog: Progress<'_>,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.enter(Op::CopyFile, src)?;
        // The wrapper is the destination for the streaming copy so that write faults apply.
        crate::copy::copy_stream(self, src, self, dst, o, prog, cancel)
    }

    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.enter(Op::Rename, to)?;
        self.inner.rename(from, to, replace)
    }

    fn delete(&self, p: &VPath, mode: DeleteMode) -> Result<()> {
        self.enter(Op::Delete, p)?;
        self.inner.delete(p, mode)
    }

    fn create_dir(&self, p: &VPath) -> Result<()> {
        self.enter(Op::CreateDir, p)?;
        self.inner.create_dir(p)
    }

    fn set_times(&self, p: &VPath, mtime: Option<Ticks>, ctime: Option<Ticks>) -> Result<()> {
        self.enter(Op::SetTimes, p)?;
        self.inner.set_times(p, mtime, ctime)
    }

    fn set_attrs(&self, p: &VPath, attrs: u32) -> Result<()> {
        self.enter(Op::SetAttrs, p)?;
        self.inner.set_attrs(p, attrs)
    }

    fn file_id(&self, p: &VPath) -> Result<Option<(u64, u128)>> {
        self.inner.file_id(p)
    }

    fn dir_case_sensitive(&self, dir: &VPath) -> Result<bool> {
        self.inner.dir_case_sensitive(dir)
    }

    fn storage_id(&self) -> usize {
        self.inner.storage_id()
    }

    fn ancestor_ids(&self) -> core_base::Result<Vec<(u64, u128)>> {
        self.inner.ancestor_ids()
    }
}
