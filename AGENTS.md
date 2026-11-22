# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository.
Read this first, then `plan.md`, which is the master design. When this file and `plan.md`
disagree, `plan.md` wins. Fix this file in the same change.

Work is tracked in `backlog.md` (epics → stories/enablers → subtasks, with order,
dependencies and acceptance criteria). Pick the lowest-order item whose dependencies are
`Done`. Meet its acceptance criteria and the backlog's Definition of Done, and update its
status in the same change.

## What this project is

`diffsync-core` is a Rust engine for text compare, folder compare, folder sync and file
operations, exposed through a C ABI (`diffsync_core.dll`). The desktop UI, **DeltaForge**, is a
separate later project written in **Avalonia (C#)**. The product command-line program is also
later and is the owner's; what it calls lives here (`core-tools`: diff command lines, tool
invocations, ignore lists, merge stub, the Explorer selection). The Windows Explorer context-menu
handler (`crates/shell-ext`) lives here too, as a thin COM layer over `core-tools::shell`.

- **v1** is a faithful baseline of the mechanics in `reference_source/MECHANICS.md`, plus
  the blueprint fixes B1–B8 and our decisions D1–D18 (`plan.md` §0.3, §11.2).
- **v2** improvements (xxHash3, order-aware closeness, move detection, tree-sitter
  weights, sync baseline DB, BLAKE3, FastCDC, remote VFS) are **out of scope** until v1 is
  done. Don't add them, even behind flags, unless asked.

## Source of truth (precedence)

1. Blueprint fixes **B1–B8** (`plan.md` §0.3)
2. `reference_source/MECHANICS.md` + `mechanics.json` (the spec)
3. Our decisions **D1–D18** (`plan.md` §11.2)
4. `reference_source/src/Dc.*.pas`: readable Delphi reference, never compiled; known to
   deviate in places
5. The rest of `reference_source/rust-source-blueprint.md`: a draft; port with review
   (`plan.md` Appendix C)
6. `reference_source/fastcopyw.cpp`: ideas only

`reference_source/Glob/` (C#, BSD-3, the owner's own library) is the reference for path,
file-system and filter behaviour. It is ported to `crates/core-glob` with the corrections
listed in `plan.md` §7.5. Don't edit anything under `reference_source/`.

If you find a new case the spec doesn't settle, don't guess silently. Add a `D19+` row to
`docs/decisions.md` (and `plan.md` §11.2) with the choice, the reason and a pinning test.

## Repository layout

```
crates/core-base     errors, cancel/pause, progress, CRC-32, FILETIME ticks
crates/core-text     decode, line index, line rules, LineMeta
crates/core-diff     aligner (Standard/Patience/Myers/Histogram/None), exact engines, text session,
                     patch output (udiff), patch parsing and applying (patch), SEARCH/REPLACE blocks,
                     the edit model (edit: EditDoc, Editor, range edits)
crates/core-vfs      Vfs trait, LocalWin32, LocalStd, MemVfs, FaultVfs, copy engine, root parsing
crates/core-glob     glob patterns, brace expansion, include/exclude rule sets, walker
crates/core-folder   folder compare tree, rules, content compare, views
crates/core-sync     sync plan/execute, file operations (copy, move, delete, rename), execution log,
                     saving an edited file (save)
crates/core-tools    CLI/MCP support: diff args + run (GNU/git output), external-tool protocol, ignore
                     lists, Explorer selection and menu logic, three-way merge stub, applying patches
                     to files (apply), JSON Patch, byte compare, git mailbox output
crates/core-ffi      C ABI → diffsync_core.dll (+ cbindgen header, csbindgen C# bindings)
crates/shell-ext     diffsync_shell.dll: Explorer IExplorerCommand handler (COM glue only)
scripts/             build-core.ps1/.cmd (DLL, shell ext, dist/), shell/ registration scripts
shell/               AppxManifest.xml (sparse package for the Windows 11 menu)
tools/dsdev          developer CLI (debug, golden, render) — not the product CLI
tools/xtask          codegen, binding checks, glob-diff, benches
tests/               fixtures, golden values, review corpus, C smoke test
docs/                spec, decisions ledger, ABI notes
```

Dependencies point strictly downward:
base → text/vfs → diff/glob → folder → sync, tools → ffi (shell-ext sits on tools).
`core-diff` must not touch the file system.

## Commands

```powershell
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo deny check                      # licenses, advisories
cargo xtask bindings                  # regenerate include/diffsync.h + bindings/csharp
cargo xtask bindings --check          # CI: fail if generated files are stale
cargo xtask glob-diff                 # core-glob vs the C# Glob library
cargo bench -p core-diff              # criterion; budgets in plan.md §12.2
cargo xtask bench                     # every budget of plan.md §12.2 (fails on a miss)
scripts/build-core.cmd -Shell         # DLL + shell extension into dist/ (see docs/integration.md)
cargo run -p dsdev -- golden all      # golden values (plan.md §11.3)
cargo run -p dsdev -- render <case> --html
```

Before you say a change is done, run `fmt`, `clippy` (no warnings) and the tests of every
crate you touched. Report failures as they are; don't skip or weaken a test to get green.

## Code rules

- **Trace every behaviour.** Cite its reason in a short comment: `// spec: ALIGN-1.4`,
  `// fix: B1`, `// decision: D7`. Don't cite addresses from the mechanics sheet.
- **Exact arithmetic in the aligner.** Integer only, truncating division, constants from
  `plan.md` Appendix A in `core-diff::align::consts`. Scores are `i32`; run weights are
  `i64`.
- **No per-line allocation.** No `String` per line, no `Cow` per line in hot paths. Reuse
  scratch buffers (`AlignScratch`).
- **No exceptions as control flow and no unbounded recursion.** Return enums; use explicit
  stacks.
- **`unsafe`** is allowed only in `core-vfs::local_win32`, `core-text` (the `mmap` module and
  the one-line `GetACP` declaration in `ansi`), `core-ffi` and `shell-ext` (COM). Every block gets
  a `// SAFETY:` comment.
- **Errors**: `core_base::Error` with an `ErrorKind`. Keep OS error codes and paths. No
  `unwrap()`/`expect()` outside tests, except where the invariant is proven and commented.
- **Threads**: never block the caller of a read function (`get_rows`, `status`). Long work
  runs on session workers or the `cpu`/`io` pools. Never call back into the UI and wait.
- **Paths**: never build paths by string concatenation; use `VPath`/`Name`. Win32 calls take
  `\\?\` paths. Never normalise names that came from a listing (trailing dots and spaces
  are real). Never use the process current directory.
- **Case and Unicode**: name pairing and filters use D6 folding; text ignore-case uses D5.
  Never use `to_lowercase()` on comparable text.

## Safety rules for anything that writes or deletes (`plan.md` §9)

These are non-negotiable. Every one has a test, and a change that weakens one needs
explicit owner approval.

1. Re-check source and destination fingerprints right before each action
   (`ChangedSinceCompare`).
2. Overwrites go through safe copy (`.dctmp~` + atomic rename). A cancel or crash never
   leaves a half-written file.
3. In a file-vs-folder conflict, the delete runs before the copy.
4. Under active filters, never delete files the user filtered out.
5. Never traverse a reparse point when deleting or copying. Deleting a junction removes
   the junction only.
6. Same or nested roots: compare may warn, sync must refuse (§9.7).
7. Validate destination names before writing (§9.8).

Tests that create, modify or delete files must use `tempfile` directories or `MemVfs`.
Never point them at real user folders.

## C ABI rules (`plan.md` §10)

- `extern "C"`. Every export is wrapped in the panic barrier and returns `DsStatus`.
  Handles are generation-checked `u64`.
- User-data strings are UTF-16 (`ptr` + `len`); options and reports are UTF-8 JSON.
- **Never renumber enums or reorder struct fields.** Add fields at the end of input
  structs (`struct_size` first). Additive changes bump the minor ABI version.
- After any ABI change, run `cargo xtask bindings`, commit the regenerated header and C#
  file, and update `tests/c-smoke` size assertions.

## Tests you are expected to add

- Unit tests for every formula branch (closeness, weights, `times_equal`, glob tokens).
- A golden case for every alignment behaviour you touch (`tests/golden`, `plan.md` §11.3).
  If your change alters a golden value, re-derive it by hand first. Don't just update the
  snapshot.
- A pinning test for every B/D item you implement.
- A proptest invariant when you add an algorithm (ops cover each line once, in order, …).
- Fault-injection tests (`FaultVfs`) for every new write path.

## Project skill and reviewer

- **Golden cases**: follow `.claude/skills/golden-case/SKILL.md` (Claude Code loads it as
  the `golden-case` skill; other agents can read it directly). Expected values come from
  the spec plus the independent reference model:
  `python -I .claude/skills/golden-case/scripts/refmodel.py selftest | align | hash | closeness`.
  Never generate expectations by running the engine.
- **Review**: before closing a milestone, and for every change to `core-vfs`, `core-sync`,
  `core-ffi` or the aligner, run the read-only `plan-reviewer` agent
  (`.claude/agents/plan-reviewer.md`) on the diff. Resolve every BLOCKER and MAJOR
  finding, or record why not. Other agents can use that file as a review checklist.

## Platform notes

- Primary target is Windows 11 x64 (`x86_64-pc-windows-msvc`). CI also builds ARM64
  Windows and runs Linux/macOS tests with `LocalStd`, so keep Windows-only code behind
  `#[cfg(windows)]` in `core-vfs`.
- The dev machine has no Delphi compiler; the `.pas` files are reading material only.
- Fuzzing: `cargo +nightly fuzz build|run <target>` from `fuzz/`. On Windows the ASAN runtime
  (`clang_rt.asan_dynamic-x86_64.dll`, in the MSVC `bin/Hostx64/x64` folder) must be on `PATH`
  to *run* a target. Miri: `cargo +nightly miri test -p core-base` (set
  `MIRIFLAGS=-Zmiri-disable-isolation` for tests that touch the file system).

## Git

- Branch from `main`. Keep commits focused, and reference plan sections or B/D IDs in the
  message (`core-diff: window slide rule (D1)`).
- Don't commit generated bindings without running `cargo xtask bindings --check`.
- Don't commit anything under `target/`, review-corpus HTML output, or files from users'
  disks.
