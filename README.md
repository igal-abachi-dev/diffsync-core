<div align="center">

# diffsync-core

**A fast, safe, embeddable engine for text compare, folder compare and folder sync.**

Written in Rust, exposed through a small, stable C ABI, and built to power desktop
compare tools.

[![CI](https://github.com/OWNER/diffsync-core/actions/workflows/ci.yml/badge.svg)](https://github.com/OWNER/diffsync-core/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/rust-2024_edition-orange)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-blue)
![ABI](https://img.shields.io/badge/C%20ABI-1.0-green)
[![License](https://img.shields.io/badge/license-TBD-lightgrey)](#license)

[Features](#features) · [Quick start](#quick-start) · [Architecture](#architecture) ·
[C ABI](#c-abi) · [Building](#building) · [Design](#design-notes) · [Roadmap](#roadmap)

</div>

---

`diffsync-core` is the rust engine behind **DeltaForge**, a desktop UI(avalonia) compare and sync tool.
It does the heavy lifting (line alignment, word-level highlighting, folder trees with
millions of entries, crash-safe copying) and leaves presentation to the UI. The UI talks
to it through one DLL with a small, versioned C interface, so the same engine can serve
an Avalonia app, a native shell extension, a CLI or your own tool.

## Features

### Text compare
- **Five alignment modes**:
  - **Standard**: a sliding window with weighted runs of matching lines, plus *closeness*
    matching that pairs similar (not just identical) lines side by side;
  - **Patience**;
  - **Myers O(ND)** (minimal);
  - **Histogram**;
  - **Unaligned**.
- **Progressive results.** Rows are published window by window. The first screen of a
  200 MB file shows while the rest is still aligning.
- **Rules**: ignore case, four whitespace modes, regex "unimportant text", and built-in
  comment rules for common languages. Differences only in unimportant text show as
  *unimportant*, not *important*.
- **Word-level highlighting**, computed on demand for visible rows only.
- **Encodings**: UTF-8 (with or without BOM), UTF-16 LE/BE, and ANSI code pages. Mixed
  CRLF/LF/CR line endings are preserved on save.
- Sections, statistics, next/previous difference, and a minimap overview.

### Folder compare
- Aligned folder trees with **size, timestamp, attribute and content** criteria:
  - timestamp tolerance, and ignoring DST or time-zone shifts (quarter-hour aware);
  - content compare by CRC-32, byte-by-byte, or rules-based text compare (so CRLF-vs-LF
    files show as *unimportant* differences).
- **Unicode-aware name pairing**: NFC/NFD, and case folding the way the file system does
  it, including case-sensitive NTFS folders.
- **Glob filters**: `**`, `{a,b}`, `[a-z]`, `/`-anchored paths, `name/` for folders only,
  and `!` to re-include. Excluded folders are never opened.
- Correct handling of **links and reparse points**:
  - symlinks, junctions and WSL links are recognised and never followed by default;
  - OneDrive, deduplicated and compressed folders are treated as normal folders;
  - online-only cloud files are never downloaded just to compare them.
- Streaming, parallel scans with folder counters kept up to date.

### Sync and file operations
- Presets: **Update Left / Right / Both** and **Mirror to Left / Right**, with per-item
  overrides.
- **Preview first.** A sync builds an immutable plan with exact counts and bytes, and
  conflicts are shown before anything is written.
- **Crash-safe**:
  - every overwrite goes to a temp file and is then atomically renamed;
  - files are re-checked right before each action;
  - cancel, pause and retry/skip/abort are supported;
  - every run writes a JSON-lines execution log.
- Long paths (`\\?\`), read-only/hidden/system files, Recycle Bin deletes, and Windows
  name rules (reserved names, invalid characters, case collisions) are checked in the
  preview.

### Engine qualities
- No per-line heap allocation. Compact arenas: about 16 bytes per text line and about
  160 bytes per folder entry.
- The UI thread never blocks: reads take microseconds, and work runs on worker pools.
- A panic-safe C ABI with generation-checked handles. A buggy caller gets an error code,
  not a crash.

## Quick start

### Rust

```toml
[dependencies]
core-diff   = { git = "https://github.com/OWNER/diffsync-core" }
core-folder = { git = "https://github.com/OWNER/diffsync-core" }
core-sync   = { git = "https://github.com/OWNER/diffsync-core" }
core-vfs    = { git = "https://github.com/OWNER/diffsync-core" }
```

**Compare two text files**

```rust
use core_diff::session::{DiffRow, DocSpec, EventSink, TextCompare, TextOptions};

let cmp = TextCompare::start(
    DocSpec::path("left.txt"),
    DocSpec::path("right.txt"),
    TextOptions::default(),          // Standard alignment, skew 2000, closeness on
    EventSink::none(),
)?;

// Rows arrive progressively; here we just wait for the end.
while !cmp.status().is_finished() {
    std::thread::sleep(std::time::Duration::from_millis(10));
}

let stats = cmp.stats();
println!("{} important difference lines in {} sections",
         stats.important_diff, stats.important_sections);

let mut rows = vec![DiffRow::default(); 50];
let n = cmp.rows(0, &mut rows);
for row in &rows[..n] {
    println!("{:?}  {:?} | {:?}", row.kind, row.left, row.right);
}
```

**Compare two folders and preview a mirror**

```rust
use core_folder::{FolderCompare, FolderRules};
use core_sync::{SyncPreset, SyncRules};
use core_vfs::LocalVfs;

let vfs = LocalVfs::shared();
let mut rules = FolderRules::default();
rules.exclude_folders = vec!["node_modules".into(), ".git".into()];

let cmp = FolderCompare::start(vfs.clone(), r"C:\work\site", vfs, r"E:\backup\site",
                               rules, EventSink::none())?;
cmp.wait()?;

let plan = core_sync::build_plan(&cmp, &SyncRules::from_preset(SyncPreset::MirrorToRight), None)?;
println!("{}", plan.summary());      // e.g. "copy 412 (1.8 GB) → right, delete 14 on right, 2 conflicts"

// Nothing has been written yet. Execute only after the user approves the plan:
// let job = plan.execute(Default::default(), EventSink::none())?;
```

### C# (Avalonia / .NET)

The `DiffSync.Native` package wraps the C ABI with `SafeHandle`s, typed exceptions and
events marshalled to the UI thread.

```csharp
using DiffSync.Native;

using var cmp = TextCompare.Open(@"C:\a\Program.cs", @"C:\b\Program.cs",
                                 new TextOptions { Alignment = Alignment.Standard });

cmp.RowsAvailable += (_, e) => Dispatcher.UIThread.Post(() => view.RowCount = e.RowCount);

Span<DsTextRow> rows = stackalloc DsTextRow[100];
int n = cmp.GetRows(firstRow: 0, rows);
```

### C

```c
#include "diffsync.h"

DsHandle cmp;
DsStatus st = ds_text_open(left16, right16, opts_json, on_event, user_ctx, &cmp);
if (st != DS_OK) { char msg[512]; size_t need; ds_last_error(msg, sizeof msg, &need); /* … */ }

DsTextRow rows[100]; uint32_t got;
ds_text_get_rows(cmp, 0, 100, rows, &got);
ds_close(cmp);
```

## Architecture

```
┌──────────────────────── DeltaForge (Avalonia, C#) ───────────────────────────┐
│        DiffSync.Native: generated bindings + SafeHandles + events            │
└────────────────────────────────────┬─────────────────────────────────────────┘
                                     ▼  C ABI — diffsync_core.dll
┌──────────────────────────────────────────────────────────────────────────────┐
│ core-ffi     handles · status codes · panic barrier · events · JSON options │
├──────────────────────────────────────────────────────────────────────────────┤
│ core-sync    plan (preview) · execute · prompts · retry/skip/abort · logs   │
│ core-folder  aligned tree · rules · timestamps · content compare · views    │
│ core-glob    glob patterns · brace expansion · include/exclude rules        │
│ core-diff    aligners · exact engines · text session · intraline · verdict  │
│ core-text    decoding · line index · line rules · line hashing              │
│ core-vfs     Vfs trait · Win32 · std · in-memory · fault injection · copy   │
│ core-base    errors · cancel/pause · progress · CRC-32 · time               │
└──────────────────────────────────────────────────────────────────────────────┘
```

| Crate | Responsibility |
|---|---|
| `core-base` | Errors, cancellation, progress, CRC-32, FILETIME ticks |
| `core-text` | Encoding detection, line index, whitespace/case/regex rules, per-line metadata |
| `core-diff` | Line alignment, Myers/Histogram/Patience, text compare sessions, word-level diff |
| `core-vfs` | File-system abstraction: Windows backend, portable backend, test backends, copy engine |
| `core-glob` | Glob matching and ignore rules (port of the Globbing C# library) |
| `core-folder` | Folder compare trees, classification, content compare, filtered/sorted views |
| `core-sync` | Sync presets, plans, execution, file operations |
| `core-ffi` | The `diffsync_core.dll` C ABI, plus generated C header and C# bindings |

**Threading.** Each session owns its worker. Shared pools handle CPU work (rayon) and
I/O. Read calls never wait on work. Callbacks are only wake-up signals, and the UI pulls
state when it is ready. Questions (conflicts, errors) are answered asynchronously, so the
engine never blocks inside a UI callback.

## C ABI

| Topic | Contract |
|---|---|
| Calling convention | `extern "C"` (x64 / ARM64) |
| Handles | `uint64_t`, generation-checked. Stale or double-closed handles return `DS_E_INVALID_HANDLE` |
| Errors | Every call returns `DsStatus`; `ds_last_error` gives a thread-local message |
| Strings | Paths, names and line text as UTF-16 (`ptr`, `len`); options and reports as UTF-8 JSON |
| Buffers | Caller-allocated, with a two-call pattern (`DS_E_BUFFER_TOO_SMALL` + `needed`) |
| Threads | Every function may be called from any thread; callbacks arrive on worker threads |
| Versioning | `ds_abi_version()`; additive changes only within a major version; input structs carry `struct_size` |

Function groups: `ds_text_*` (compare, rows, lines, intraline, sections, navigation),
`ds_folder_*` and `ds_view_*` (trees and flat views for virtualised grids), `ds_sync_*` /
`ds_plan_*` (plans), and `ds_job_*` / `ds_fileops_*` (execution, pause, cancel,
questions). The full reference is in [`docs/abi.md`](docs/abi.md); the header is
[`include/diffsync.h`](include/diffsync.h).

## Building

Requirements: Rust stable (pinned in `rust-toolchain.toml`). On Windows, the MSVC build
tools.

```powershell
git clone https://github.com/OWNER/diffsync-core
cd diffsync-core
cargo build --release -p core-ffi        # → target/release/diffsync_core.dll
cargo test --workspace
```

| Task | Command |
|---|---|
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Regenerate C header + C# bindings | `cargo xtask bindings` |
| Benchmarks | `cargo bench -p core-diff` |
| Golden tests | `cargo run -p dsdev -- golden all` |
| Side-by-side review page | `cargo run -p dsdev -- render <case> --html` |
| License / advisory audit | `cargo deny check` |

The release DLL is built with a static CRT, so it has no Visual C++ runtime dependency.

## Testing

| Layer | What it covers |
|---|---|
| Unit | Every scoring formula, weight, hash vector, timestamp rule, glob token |
| Golden | Hand-derived alignment results (op streams and rows), folder, filter and path cases |
| Property | Every line appears once, in order; identical inputs give all-same; mirrored inputs give mirrored kinds |
| Differential | Myers vs brute-force LCS and `imara-diff`; `core-glob` vs the reference C# library |
| Fuzzing | Decoders, aligner at tiny window sizes, options parser, FFI entry points |
| Fault injection | Failures at every write step, cancel mid-copy, files changed between preview and execute |
| Review corpus | ~60 real-world file pairs, rendered side by side and locked as approved snapshots |
| ABI | A C program checks struct layouts, handle misuse, buffer sizing and the panic barrier |

## Performance

Budgets enforced by the benchmark suite on the reference machine (8 cores, NVMe,
Windows 11). A regression of more than 10% fails CI.

| Scenario | Budget |
|---|---|
| Line indexing | ≥ 1 GB/s |
| 2 × 100 MB near-identical logs: first 200 rows | ≤ 100 ms |
| Same, full alignment | ≤ 1.5 s |
| Viewport pull (100 rows + text + intraline) | ≤ 0.2 ms |
| Folder scan, warm cache | ≥ 150 000 entries/s |
| 1 M folder entries in memory | ≤ 160 MB |
| Large file copy | within 5% of `robocopy` |

## Design notes

<details>
<summary><b>Standard alignment</b></summary>

Lines are hashed after the rules are applied (whitespace, case, unimportant text). A
weight marks how much each line matters: 2 for empty lines, 31 for lines with no
important text, 70 for normal lines. The aligner then:

1. pairs a common prefix, collecting blank or insignificant lines into a pending gap;
2. slides a 2000-line window over the rest, finding diagonal runs of equal lines; runs
   lighter than 500 are ignored;
3. takes a run that reaches 5000 immediately, which is why the first screen appears at once
   even for huge files;
4. keeps the heaviest non-crossing runs and fills the gaps between them with a
   Needleman–Wunsch pass scored by *closeness* (a byte-overlap similarity), so edited lines
   sit across from each other instead of becoming delete + insert;
5. emits long one-sided blocks in chunks, so the UI stays responsive.

Every constant and rule is documented in `docs/spec/` and pinned by golden tests.
</details>

<details>
<summary><b>Crash-safe sync</b></summary>

- **Plan, then execute.** The plan is immutable and fully shown before execution.
- **Re-check.** Each action re-reads both sides. Anything changed since the compare
  becomes a question, never a silent overwrite.
- **Atomic replace.** Data goes to `<name>.<tag>.dctmp~`, then `MoveFileExW` with
  replace-existing and write-through.
- **Ordering.** Conflicting deletes, then folder creation, copies, remaining deletes
  (children first), and finally folder timestamps.
- **Guards.** Same or nested roots block sync. Filtered-out files are never deleted.
  Junctions are deleted as links, never followed.
</details>

<details>
<summary><b>File-system edge cases handled</b></summary>

Long paths · UNC roots · drive-relative paths rejected · names with trailing dots/spaces ·
reserved device names · invalid-on-Windows names from other systems · alternate-data-stream
traps · case-sensitive NTFS folders · case-insensitive macOS volumes · NFC/NFD names ·
unpaired surrogates in names · symlinks vs junctions vs cloud placeholders · link loops
detected by file ID · folders vanishing mid-scan · access-denied subfolders.
</details>

## Platform support

| Platform | Status |
|---|---|
| Windows 10/11 x64 | Primary; full Win32 backend |
| Windows 11 ARM64 | Built and tested in CI |
| Linux, macOS | Engine and portable `std` backend; tested in CI |

## Roadmap

- Editing inside a text compare, with incremental re-alignment and save
- Hex compare and 3-way text merge
- **v2 engine**:
  - 64-bit line hashes;
  - order-aware closeness;
  - global anchors and moved-block detection;
  - structure-aware weights (tree-sitter);
  - two-way sync with a baseline database;
  - BLAKE3 content mode and rename detection;
  - delta copy;
  - SFTP / S3 / WebDAV / archive back ends.

## Contributing

Contributions are welcome. Please read [`AGENTS.md`](AGENTS.md) for the coding rules,
safety rules and test expectations, and [`plan.md`](plan.md) for the full design. Every
behaviour change needs a test, and any change to a write path needs a fault-injection test.

## License

<!-- TODO: choose a license (e.g. MIT, Apache-2.0, BSD-3-Clause) and add a LICENSE file. -->
License to be announced. Third-party crates are listed in `Cargo.lock` and audited with
`cargo deny`.
