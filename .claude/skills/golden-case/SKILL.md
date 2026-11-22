---
name: golden-case
description: Add, verify or debug a golden test case for diffsync-core. Covers text alignment ops and rows, line hashes, closeness scores, folder-compare statuses, glob filters and path parsing (plan.md §11.3). Use it when writing a new golden fixture, when deriving expected values for a behaviour (B1–B8 fixes, D1–D18 decisions, spec rules), when a golden test fails and you must decide whether the engine or the expectation is wrong, or when a new edge case needs a decision-ledger entry.
---

# Golden cases for diffsync-core

Golden values are the main correctness check of v1. There is no BC5 oracle, so every
expected value must be **derived from the spec independently of the engine**, and then
checked with the reference model. Never produce an expected value by running the Rust
engine and copying its output.

Background reading, only the parts you need: `plan.md` §5.2 (aligner, window rule D1,
closeness D2), §11.2 (decisions), §11.3 (existing golden values), §6.4 / §7.5 (paths and
filters), and `reference_source/MECHANICS.md` §1–§3.

## Tool: the reference model

`scripts/refmodel.py` is an independent Python model of the spec. Always run it with
`python -I` (isolated mode). Commands:

```
python -I .claude/skills/golden-case/scripts/refmodel.py selftest
python -I .claude/skills/golden-case/scripts/refmodel.py hash "text" ...
python -I .claude/skills/golden-case/scripts/refmodel.py closeness "A" "B" [--same-only]
python -I .claude/skills/golden-case/scripts/refmodel.py align LEFT RIGHT [--options options.json] [--json]
        [--kind standard|none] [--skew N] [--whitespace all|leadingTrailing|changes|ignoreAll]
        [--ignore-case] [--unimportant REGEX ...] [--orphans-sig] [--no-closeness]
        [--same-only] [--never-align]
```

Run `selftest` first in every session. It must print `selftest ok`. If it fails, the model
or its copy is broken: stop and report it, don't use it.

**What the model covers:**
- Line hash and file CRC.
- Rules: whitespace modes, blank set, ignore-case, regexes.
- Weights and closeness (D2).
- Standard and None alignment: top-level walk with pending gap, orphan chunks of 200, the skew
  window with the L-shaped scan, diagonal runs (weight > 500), run selection with the B3
  clipping and the D1 rule, big runs (B1 extension, B2 block before the run), gap DP with
  partial traces.
- Row kinds.
- Event counters (`STATS`: clip, trim_head, trim_tail, drop, big_run, slide_commit, partial, ...)
  that show which parts of the window machinery a case exercises.

**What it does not cover.** In these cases it prints `OUT OF SCOPE` (exit code 2), and you
derive by hand (step 3b):
- Patience, Myers and Histogram;
- custom weights.

The model and the Rust aligner are two implementations of the same spec written separately.
`tests/differential/align_vs_refmodel.py` runs them against each other on random structured
inputs; run it after any change to either. A hand-derived case for runs, clipping, selection
or big runs must still be derived by hand first (step 3b); the model is then the cross-check,
and `tests/fixtures/text/gen_golden.py` asserts that the two agree before writing the fixture.

**Known approximations:**
- Ignore-case uses Python `lower()` per code point instead of exact Unicode *simple*
  folding (D5). Avoid exotic letters in ignore-case cases, or check them by hand.
- Unimportant regexes use Python `re` syntax. Keep patterns to the common subset shared
  with the Rust `regex` crate.

## Procedure

### 1. Pick the category and the behaviour being pinned

| Category | Fixture location | Expected files |
|---|---|---|
| Text alignment | `tests/fixtures/text/<case>/` | `left`, `right`, `options.json`, `expected.ops`, `expected.rows` |
| Hash / closeness vectors | `crates/core-base/tests/vectors.rs`, `crates/core-diff/tests/closeness.rs` | inline tables |
| Folder compare | `tests/fixtures/folders/<case>.toml` (MemVfs recipe: entries, sizes, mtimes, attrs) | `expected.status` |
| Glob filters | `crates/core-glob/tests/golden.rs` | inline table: rules, path, is_dir, expected |
| Paths | `crates/core-vfs/tests/paths_golden.rs` | inline table: input, expected root / long form / error |

Name the case after what it pins, with the ID when there is one: `b1_big_run_crosses_window`,
`d2_whitespace_twin_preferred`, `d7_nepal_offset`.

### 2. Build the smallest input that isolates the behaviour

- Keep inputs tiny unless size *is* the behaviour (windows, chunks, big runs).
- Make the expected output independent of incidental text:
  - **Lines that must never look alike**: build them from disjoint byte sets (left from
    `[a-m]`, right from `[n-z]`) with length ≥ 10. Every such pair then scores exactly −40,
    every DP path ties, and the pair-first tie rule gives the diagonal.
  - **Lines that must pair as Different**: share most bytes (`int x = 1;` / `int x = 2;`
    scores 17).
  - **Run weights**: significant lines weigh 70, so a run needs 8 lines to exceed 500 and
    72 lines to reach 5040 ≥ 5000 (instant take). Insignificant lines weigh 31, empty
    lines 2.
- `options.json` uses the option keys from `plan.md` §5.4 and README: `alignment`,
  `skewTolerance`, `useCloseness`, `closenessSameOnly`, `neverAlignDiffs`,
  `orphansAlwaysSig`, `ignoreCase`, `whitespace`, `unimportant`. Write only the keys that
  differ from the defaults.

### 3a. Derive the expected values (model in scope)

1. Write down your own expectation first, from the spec: which lines pair, which are
   orphans, and in what op order.
2. Run `refmodel.py align`.
3. If both agree, use the model's output for `expected.ops` / `expected.rows`.
4. If they disagree, find out why before writing anything. Re-check the tie order,
   pending-gap rules, D1 and D2. One of the two is wrong, and you must know which.

### 3b. Derive by hand (always for runs and big runs; the only way for Patience, Myers, Histogram)

Trace the algorithm step by step and write the trace in the test's doc comment. Checklist:

- **Top-level walk (1.3).** Pairs leading lines with equal hashes one at a time
  (`Add(i,j,1)`). Insignificant lines join the pending gap. It stops at the first pair of
  significant lines that differ.
- **Window.** It covers `skew` lines per side from the current position. The L-shaped scan
  visits cell (r,c) at step `max(r,c)`: column `j=k` from row `k` upwards, then row `i=k`
  leftwards. Along a diagonal the top-left cell is visited first, so a run is found from
  its start.
- **Runs.** A run is kept when its weight is > 500. At ≥ 5000 it is taken at once (B2):
  first align the block before it, then extend it to the **outer block stop** (B1), emitting
  pieces of 501 lines (emit when count > 500), with the remainder last.
- **Selection (B3 + D1).** Applies when `more_after`: clip runs so the window's last line
  stays out, re-weigh (< 500 → 0), take the heaviest first, trim or drop overlaps, then emit
  in position order with gap DP between runs. Commit up to the end of the last run and
  slide. Without `more_after`: no clipping, and gap-fill to the block stop.
- **Gap DP.** Gap −20. Ties prefer pair, then source orphan, then target orphan. Equal
  consecutive steps merge into one op. A partial trace stops when one side is used up.
- **Orphans.** When one side of a block is exhausted, the other is emitted in chunks of
  200 (2000 for Patience).

Where possible, cut the big-run part into a model-checkable piece. For example, verify the
header block alone with the model, then hand-derive only the run emission.

### 3c. Folder, filter and path cases

Derive from the rules, not from the code:
- `times_equal` and D7 (§7.3 step 6), name keys and D6 (§7.3 step 2), classification
  (§7.3 step 5), filters (§7.5 table), path parsing and names (§6.4).
- Glob cases may also be checked against the C# library with `cargo xtask glob-diff`, but
  only where §7.5 lists no correction.

### 4. Write the fixture and the test

- `expected.ops`: one op per line, `src tgt amount`, `-` for the orphan side
  (`- 2 1`).
- `expected.rows`: `left right Kind`, `-` for absent
  (`- 2 RightOnly`). Kinds: `Same, Unimportant, Different, LeftOnly, RightOnly,
  UnimportantLeftOnly, UnimportantRightOnly`.
- `left`/`right` files: UTF-8 unless the encoding is the point of the case. Mention any
  deliberate CRLF, missing final newline or BOM in the test's doc comment.
- The test's doc comment states the behaviour, the B/D/spec ID, and either
  "verified with refmodel" or the hand trace.

### 5. Ledger and plan

- If the case pins a B/D item, reference it in `docs/decisions.md` next to that item
  ("pinned by: `tests/fixtures/text/d2_whitespace_twin_preferred`").
- If the case exposes behaviour the spec doesn't settle, add a new `D19+` row (decision,
  reason, pinning test) to `docs/decisions.md` and `plan.md` §11.2. Ask the user when the
  choice changes visible results in a way they might not want.
- Add notable cases to the `plan.md` §11.3 tables.

## When an existing golden test fails

1. **Never** update `expected.*` just to make the test pass.
2. Re-run `refmodel.py selftest`, then the model on the case (if in scope).
3. Re-derive by hand from the spec.
4. Then:
   - The engine is wrong → fix the engine.
   - The expectation was wrong → fix it, and record in the commit message why the old
     value was wrong.
   - The spec is ambiguous → new D row, and confirm with the user before changing an
     existing expectation.

## Keeping the model correct

When the spec changes or a new D decision affects text alignment, update
`scripts/refmodel.py` and its `selftest` in the same change. The selftest must always
contain every §11.3 value the model covers.
