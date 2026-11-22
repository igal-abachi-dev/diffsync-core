# Review corpus sources

Every pair in this folder is **generated** by `gen_corpus.py` from fixed random seeds and small
templates (refactors, re-indentation, reordered JSON, CSV edits, Hebrew/English text, minified
JS, `}` / `end;` storms, moved blocks, logs, comments, line endings, alignment kinds,
degenerate inputs). No private, customer or third-party files are included, so there is nothing
to license. To add a real-world pair, put it in its own folder, record its origin and license in
the table below, and make sure it is permissively licensed.

| Folder | Origin |
|---|---|
| all folders produced by `gen_corpus.py` | generated, no external source |

The plan's 100k-line log and full-size moved-block cases are generated at reduced size by
default; `python -I tests/review/gen_corpus.py --big` writes the full-size variants.

## Approval

`crates/core-diff/tests/review_corpus.rs` stores the rows of every case as an `insta` snapshot
in `crates/core-diff/tests/snapshots/`. A snapshot counts as approved when
`python -I tests/review/approve.py` passes: every line is covered exactly once and in order, the
rows of every Standard/None case equal the output of the independent Python reference model
(`.claude/skills/golden-case/scripts/refmodel.py`), and Same rows hold identical lines under
default options. Myers, Histogram and Patience cases are not modelled and pass the structural
checks only (their match counts are pinned by `tests/exact_engines.rs` and `tests/align_kinds.rs`).

**Approved 2026-11-22** at the owner's request ("try approve yourself"): 60 cases, 57 of them
cross-checked against the model, 0 differences. After any change to the aligner, re-run the
script; a difference must be understood before a snapshot is accepted (golden-case skill).
