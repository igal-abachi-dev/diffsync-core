//! The text compare session (plan.md §5.4, backlog E4-S1, S2, S4, S6, S8).
//!
//! `TextCompare::start` returns at once. Loading and alignment run on a worker thread that
//! classifies the ops into rows and publishes them in batches (at least 256 rows or every
//! 16 ms). Readers (`rows`, `status`, `stats`, ...) only take short read locks.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use core_base::{CancelToken, Error, Result};
use core_text::{LineRules, LoadOptions, SourceId, TextDocument};
use parking_lot::{Mutex, RwLock};

use super::intraline::{IntraCache, IntraLine, intraline_diff};
use super::rows::{DiffRow, Dir, RowKind, RowStore, Section, Stats};
use crate::align::{
    AlignOp, AlignRange, AlignScratch, DocSource, ORPHAN, Side, align_with_scratch,
};
use crate::options::TextOptions;

/// Rows per published batch (or every [`BATCH_INTERVAL`]).
pub const BATCH_ROWS: usize = 256;
pub const BATCH_INTERVAL: Duration = Duration::from_millis(16);
/// Wake-ups are coalesced to this interval (about 60 Hz).
pub const EVENT_INTERVAL: Duration = Duration::from_millis(16);

/// Event bits delivered to the sink.
pub mod event {
    /// New rows or statistics are available.
    pub const ROWS: u32 = 1;
    /// The status changed.
    pub const STATE: u32 = 2;
}

/// A wake-up callback, called from the worker thread. It must return quickly; the UI then
/// pulls state. Bits are in [`event`].
pub type EventSink = Arc<dyn Fn(u32) + Send + Sync>;

/// Where a document comes from.
#[derive(Debug, Clone)]
pub enum DocSpec {
    Path(PathBuf),
    /// Raw file bytes (encoding detection applies).
    Bytes(Vec<u8>),
    /// UTF-8 text.
    Text(String),
    Loaded(Arc<TextDocument>),
}

impl DocSpec {
    fn load(&self, opts: &LoadOptions) -> Result<Arc<TextDocument>> {
        Ok(match self {
            DocSpec::Path(p) => Arc::new(TextDocument::load(p, opts)?),
            DocSpec::Bytes(b) => Arc::new(TextDocument::from_bytes(b.clone(), &opts.decode)?),
            DocSpec::Text(t) => Arc::new(TextDocument::from_text(t)?),
            DocSpec::Loaded(d) => d.clone(),
        })
    }
}

/// The state of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Loading,
    /// Aligning; `rows` have been published so far.
    Aligning {
        rows: u32,
    },
    Done,
    Failed {
        error: String,
    },
    Cancelled,
    /// Either side is a binary file; there are no rows (plan.md §5.5).
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Loading,
    Aligning,
    Done,
    Failed(String),
    Cancelled,
    Binary,
}

/// Facts about a loaded document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocInfo {
    pub encoding: String,
    pub has_bom: bool,
    /// Lines per ending: `[none, CRLF, LF, CR]`.
    pub ending_counts: [u64; 4],
    pub is_binary: bool,
    pub line_count: u32,
    pub byte_len: u64,
    pub source: SourceId,
}

/// Coalesces wake-ups to at most one per [`EVENT_INTERVAL`] and kind; whatever was held back
/// goes out with `flush`.
struct Events {
    sink: Option<EventSink>,
    state: Mutex<EventState>,
}

#[derive(Default)]
struct EventState {
    last: [Option<Instant>; 2],
    pending: u32,
}

impl Events {
    fn notify(&self, bit: u32) {
        let Some(sink) = &self.sink else { return };
        let idx = bit.trailing_zeros() as usize;
        let send = {
            let mut st = self.state.lock();
            let due = st.last[idx].is_none_or(|t| t.elapsed() >= EVENT_INTERVAL);
            if due {
                st.last[idx] = Some(Instant::now());
                st.pending &= !bit;
            } else {
                st.pending |= bit;
            }
            due
        };
        if send {
            sink(bit);
        }
    }

    /// Sends what was held back.
    fn flush(&self) {
        let Some(sink) = &self.sink else { return };
        let bits = {
            let mut st = self.state.lock();
            let b = st.pending;
            st.pending = 0;
            let now = Instant::now();
            for (i, last) in st.last.iter_mut().enumerate() {
                if b & (1 << i) != 0 {
                    *last = Some(now);
                }
            }
            b
        };
        if bits != 0 {
            sink(bits);
        }
    }
}

struct Docs {
    left: Arc<TextDocument>,
    right: Arc<TextDocument>,
}

struct Shared {
    store: RowStore,
    state: Mutex<State>,
    docs: RwLock<Option<Docs>>,
    /// The sources while the documents are not loaded yet: a `recompare` that cancels a load
    /// restarts it from these instead of failing with "no documents loaded" (reviewer, E11-S6).
    pending: Mutex<Option<[DocSpec; 2]>>,
    options: RwLock<TextOptions>,
    events: Events,
    intra: Mutex<IntraCache>,
    generation: AtomicU64,
    /// Rows published by the current worker (for `Status::Aligning`).
    aligning_rows: AtomicU32,
    load: LoadOptions,
    /// Test hook: panic in the worker after this many rows.
    panic_after: Option<u32>,
}

/// A running or finished text compare. Share it freely; all methods take `&self`.
pub struct TextCompare {
    shared: Arc<Shared>,
    cancel: Mutex<CancelToken>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for TextCompare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextCompare")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl TextCompare {
    /// Starts a compare and returns at once. The options are validated here (bad regex,
    /// contradictory settings fail with `InvalidArgument`); loading errors show up as
    /// `Status::Failed`.
    pub fn start(
        left: DocSpec,
        right: DocSpec,
        opts: TextOptions,
        events: Option<EventSink>,
    ) -> Result<Arc<Self>> {
        Self::start_with(left, right, opts, events, LoadOptions::default(), None)
    }

    #[doc(hidden)]
    pub fn start_with(
        left: DocSpec,
        right: DocSpec,
        opts: TextOptions,
        events: Option<EventSink>,
        load: LoadOptions,
        panic_after: Option<u32>,
    ) -> Result<Arc<Self>> {
        opts.line_rules()?;
        opts.align_config().resolve()?;
        let shared = Arc::new(Shared {
            store: RowStore::new(),
            state: Mutex::new(State::Loading),
            docs: RwLock::new(None),
            pending: Mutex::new(Some([left.clone(), right.clone()])),
            options: RwLock::new(opts),
            events: Events {
                sink: events,
                state: Mutex::new(EventState::default()),
            },
            intra: Mutex::new(IntraCache::default()),
            generation: AtomicU64::new(0),
            aligning_rows: AtomicU32::new(0),
            load,
            panic_after,
        });
        let this = Arc::new(TextCompare {
            shared,
            cancel: Mutex::new(CancelToken::new()),
            worker: Mutex::new(None),
        });
        this.spawn(Some([left, right]), true);
        Ok(this)
    }

    fn spawn(&self, specs: Option<[DocSpec; 2]>, bump_generation: bool) {
        let shared = self.shared.clone();
        let token = CancelToken::new();
        *self.cancel.lock() = token.clone();
        let generation = if bump_generation {
            shared.generation.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            shared.generation.load(Ordering::SeqCst)
        };
        let handle = std::thread::Builder::new()
            .name("diffsync-text-compare".into())
            .spawn(move || {
                if specs.is_none() {
                    shared.events.notify(event::STATE); // a recompare started
                }
                worker_main(&shared, specs, &token, generation);
            })
            .expect("spawn worker thread");
        *self.worker.lock() = Some(handle);
    }

    /// Counts the compare runs of this session (each `recompare` starts a new one); a row cache
    /// held by a host is stale when this number changes.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// The line ending of a line (`None` when the line or the document does not exist).
    pub fn line_ending(&self, side: Side, line: u32) -> Option<core_text::LineEnding> {
        let g = self.shared.docs.read();
        let d = g.as_ref()?;
        let doc = if side == Side::Left {
            &d.left
        } else {
            &d.right
        };
        ((line as usize) < doc.line_count()).then(|| doc.line_ending(line as usize))
    }

    /// The current status.
    pub fn status(&self) -> Status {
        match &*self.shared.state.lock() {
            State::Loading => Status::Loading,
            State::Aligning => Status::Aligning {
                rows: self.shared.store.published(),
            },
            State::Done => Status::Done,
            State::Failed(e) => Status::Failed { error: e.clone() },
            State::Cancelled => Status::Cancelled,
            State::Binary => Status::Binary,
        }
    }

    /// Stops the worker; the status becomes `Cancelled` within a few milliseconds.
    pub fn cancel(&self) {
        self.cancel.lock().cancel();
    }

    /// Waits until the worker has finished (for tests and synchronous callers).
    pub fn wait(&self) {
        let handle = self.worker.lock().take();
        if let Some(h) = handle {
            // a host callback runs on the worker: joining oneself would never return
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
    }

    /// Re-runs the alignment with new options over the same documents.
    pub fn recompare(&self, opts: TextOptions) -> Result<()> {
        opts.line_rules()?;
        opts.align_config().resolve()?;
        self.cancel();
        self.wait();
        *self.shared.options.write() = opts;
        // cancelled while still loading: there are no documents yet, so run the whole load again
        // (decision: D51 follow-up; the new options apply to it)
        let reload = self.shared.pending.lock().clone();
        if let Some(specs) = reload {
            self.shared.generation.fetch_add(1, Ordering::SeqCst);
            self.shared.store.clear();
            self.shared.intra.lock().clear();
            self.shared.aligning_rows.store(0, Ordering::Relaxed);
            *self.shared.state.lock() = State::Loading;
            self.spawn(Some(specs), false);
            return Ok(());
        }
        // the new generation is visible before the old rows go (a host that sees no rows also
        // sees that the generation changed)
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.shared.store.clear();
        self.shared.intra.lock().clear();
        self.shared.aligning_rows.store(0, Ordering::Relaxed);
        *self.shared.state.lock() = State::Aligning;
        // the STATE event comes from the new worker, never from this (the caller's) thread
        self.spawn(None, false);
        Ok(())
    }

    /// Copies display rows `first..` into `out`; never waits on the aligner for longer than
    /// one batch append.
    pub fn rows(&self, first: u32, out: &mut [DiffRow]) -> usize {
        self.shared.store.rows(first, out)
    }

    pub fn row_count(&self) -> u32 {
        self.shared.store.published()
    }

    pub fn sections(&self, first: u32, out: &mut [Section]) -> usize {
        self.shared.store.sections(first, out)
    }

    /// Live statistics, consistent with the rows published at the same instant.
    pub fn stats(&self) -> Stats {
        self.shared.store.stats()
    }

    pub fn find_diff(&self, from_row: u32, dir: Dir, important_only: bool) -> Option<u32> {
        self.shared.store.find_diff(from_row, dir, important_only)
    }

    pub fn overview(&self, buckets: &mut [u8]) {
        self.shared.store.overview(buckets);
    }

    /// The text of a line (without its ending) as UTF-16. Returns false when the document is
    /// not loaded yet or the line does not exist.
    pub fn line_utf16(&self, side: Side, line: u32, out: &mut Vec<u16>) -> bool {
        out.clear();
        let g = self.shared.docs.read();
        let Some(d) = g.as_ref() else { return false };
        let doc = if side == Side::Left {
            &d.left
        } else {
            &d.right
        };
        if line as usize >= doc.line_count() {
            return false;
        }
        out.extend(doc.line(line as usize).encode_utf16());
        true
    }

    pub fn doc_info(&self, side: Side) -> Option<DocInfo> {
        let g = self.shared.docs.read();
        let d = g.as_ref()?;
        let doc = if side == Side::Left {
            &d.left
        } else {
            &d.right
        };
        Some(DocInfo {
            encoding: doc.encoding().name().to_string(),
            has_bom: doc.has_bom(),
            ending_counts: doc.ending_counts(),
            is_binary: doc.is_binary(),
            line_count: doc.line_count() as u32,
            byte_len: doc.byte_len(),
            source: doc.source().clone(),
        })
    }

    /// Word-level differences of a paired row (computed on demand, cached). `None` for rows
    /// that are not paired, are identical, or do not exist yet.
    pub fn intraline(&self, row: u32) -> Option<Arc<IntraLine>> {
        let mut r = [DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same,
        }];
        if self.shared.store.rows(row, &mut r) == 0 {
            return None;
        }
        let r = r[0];
        let (l, rt) = (r.left_opt()?, r.right_opt()?);
        if r.kind == RowKind::Same {
            return None;
        }
        if let Some(hit) = self.shared.intra.lock().get(row) {
            return Some(hit);
        }
        let docs = self.shared.docs.read();
        let d = docs.as_ref()?;
        let rules = self.shared.options.read().line_rules().ok()?;
        let diff = intraline_diff(
            d.left.line(l as usize),
            d.right.line(rt as usize),
            &rules,
            &CancelToken::new(),
        )
        .ok()?;
        drop(docs);
        let arc = Arc::new(diff);
        self.shared.intra.lock().put(row, arc.clone());
        Some(arc)
    }
}

impl Drop for TextCompare {
    fn drop(&mut self) {
        self.cancel.lock().cancel();
        if let Some(h) = self.worker.lock().take() {
            // the last reference can be dropped on the worker itself (a callback closing the
            // session): joining oneself would never return
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
    }
}

fn set_state(shared: &Shared, generation: u64, state: State) {
    if shared.generation.load(Ordering::SeqCst) != generation {
        return; // a newer worker owns the state
    }
    *shared.state.lock() = state;
    shared.events.notify(event::STATE);
}

fn worker_main(
    shared: &Arc<Shared>,
    specs: Option<[DocSpec; 2]>,
    cancel: &CancelToken,
    generation: u64,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_worker(shared, specs, cancel, generation)
    }));
    let final_state = match result {
        Ok(Ok(state)) => state,
        Ok(Err(e)) if e.is_cancelled() => State::Cancelled,
        Ok(Err(e)) => State::Failed(e.to_string()),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            State::Failed(format!("internal error: worker panicked: {msg}"))
        }
    };
    set_state(shared, generation, final_state);
    shared.events.notify(event::ROWS);
    shared.events.flush();
}

fn run_worker(
    shared: &Arc<Shared>,
    specs: Option<[DocSpec; 2]>,
    cancel: &CancelToken,
    generation: u64,
) -> Result<State> {
    if let Some([l, r]) = specs {
        set_state(shared, generation, State::Loading);
        let (left, right) = (l.load(&shared.load)?, r.load(&shared.load)?);
        cancel.check()?;
        *shared.docs.write() = Some(Docs { left, right });
        *shared.pending.lock() = None; // loaded: the sources are not needed again
    }
    let (left, right) = {
        let g = shared.docs.read();
        let d = g
            .as_ref()
            .ok_or_else(|| Error::internal("no documents loaded"))?;
        (d.left.clone(), d.right.clone())
    };
    if left.is_binary() || right.is_binary() {
        return Ok(State::Binary);
    }
    let opts = shared.options.read().clone();
    let rules: LineRules = opts.line_rules()?;
    let cfg = opts.align_config();
    set_state(shared, generation, State::Aligning);

    let source = DocSource::new(&left, &right, &rules, None);
    let mut classifier = Classifier {
        source: &source,
        orphans_sig: opts.orphans_always_sig,
    };
    let mut batch: Vec<DiffRow> = Vec::with_capacity(BATCH_ROWS * 2);
    let mut last_flush = Instant::now();
    let mut total = 0u32;
    let flush = |batch: &mut Vec<DiffRow>, last: &mut Instant| {
        if !batch.is_empty() {
            shared.store.append(batch);
            shared
                .aligning_rows
                .store(shared.store.published(), Ordering::Relaxed);
            batch.clear();
            shared.events.notify(event::ROWS);
        }
        *last = Instant::now();
    };
    let mut scratch = AlignScratch::new();
    align_with_scratch(
        &source,
        &cfg,
        AlignRange::full(),
        cancel,
        &mut |op: AlignOp| {
            classifier.rows(op, &mut batch);
            total += op.amount;
            if let Some(limit) = shared.panic_after {
                assert!(total < limit, "injected panic after {limit} rows");
            }
            if batch.len() >= BATCH_ROWS || last_flush.elapsed() >= BATCH_INTERVAL {
                flush(&mut batch, &mut last_flush);
            }
        },
        &mut scratch,
    )?;
    flush(&mut batch, &mut last_flush);
    Ok(State::Done)
}

/// Turns ops into rows (spec: `ClassifyPair`, plan.md §5.4).
pub(super) struct Classifier<'a, 'd> {
    pub source: &'a DocSource<'d>,
    pub orphans_sig: bool,
}

impl Classifier<'_, '_> {
    /// Paired: full texts equal → Same; comparable texts equal → Unimportant; else Different.
    pub fn pair_kind(&self, l: u32, r: u32) -> RowKind {
        use crate::align::LineSource;
        let (a, b) = (
            self.source.meta(Side::Left, l),
            self.source.meta(Side::Right, r),
        );
        if self.source.full_text_eq(l, r) {
            // equal text with different endings is only a difference when endings are compared:
            // then the hashes (which carry the ending, decision D55) are not equal
            return if a.hash == b.hash {
                RowKind::Same
            } else {
                RowKind::Different
            };
        }
        let equal = match (a.is_sig(), b.is_sig()) {
            (false, false) => true, // both comparable texts are empty
            (true, true) => a.hash == b.hash && self.source.comparable_eq(l, r),
            _ => false,
        };
        if equal {
            RowKind::Unimportant
        } else {
            RowKind::Different
        }
    }

    /// An orphan is unimportant when its line is insignificant, unless `orphans_always_sig`.
    pub fn orphan_kind(&self, side: Side, idx: u32) -> RowKind {
        let important = self.orphans_sig || !self.source.is_insignificant(side, idx);
        match (side, important) {
            (Side::Left, true) => RowKind::LeftOnly,
            (Side::Left, false) => RowKind::UnimportantLeftOnly,
            (Side::Right, true) => RowKind::RightOnly,
            (Side::Right, false) => RowKind::UnimportantRightOnly,
        }
    }

    pub fn rows(&mut self, op: AlignOp, out: &mut Vec<DiffRow>) {
        for k in 0..op.amount {
            let row = match (op.src_opt(), op.tgt_opt()) {
                (Some(s), Some(t)) => DiffRow {
                    left: s + k,
                    right: t + k,
                    kind: self.pair_kind(s + k, t + k),
                },
                (Some(s), None) => DiffRow {
                    left: s + k,
                    right: ORPHAN,
                    kind: self.orphan_kind(Side::Left, s + k),
                },
                (None, Some(t)) => DiffRow {
                    left: ORPHAN,
                    right: t + k,
                    kind: self.orphan_kind(Side::Right, t + k),
                },
                (None, None) => continue,
            };
            out.push(row);
        }
    }
}

/// Compares two files byte for byte in 1 MiB blocks (shared with folder compare, E4-S8).
pub fn binary_equal(
    a: &std::path::Path,
    b: &std::path::Path,
    cancel: &CancelToken,
) -> Result<bool> {
    use std::io::Read;
    const BLOCK: usize = 1 << 20;
    fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            match r.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    }

    let open = |p: &std::path::Path| std::fs::File::open(p).map_err(|e| Error::from_io(&e, p));
    let (fa, fb) = (open(a)?, open(b)?);
    let len = |f: &std::fs::File, p: &std::path::Path| {
        f.metadata()
            .map(|m| m.len())
            .map_err(|e| Error::from_io(&e, p))
    };
    if len(&fa, a)? != len(&fb, b)? {
        return Ok(false);
    }
    let (mut ra, mut rb) = (
        std::io::BufReader::with_capacity(BLOCK, fa),
        std::io::BufReader::with_capacity(BLOCK, fb),
    );
    let (mut ba, mut bb) = (vec![0u8; BLOCK], vec![0u8; BLOCK]);
    loop {
        cancel.check()?;
        let na = read_full(&mut ra, &mut ba).map_err(|e| Error::from_io(&e, a))?;
        let nb = read_full(&mut rb, &mut bb).map_err(|e| Error::from_io(&e, b))?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}
