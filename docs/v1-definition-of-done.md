# v1 Definition of Done: evidence (backlog E11-S6.1, plan.md §13.1)

Status of each point of plan §13.1, with the command or test that shows it. "Dev machine" means
the Windows 11 x64 NVMe workstation these were run on. Things that need the GitHub CI, the owner
or a human are marked **open** and are listed again at the end.

## 1. Every §11.1 row meets its target; the review corpus is approved

| §11.1 row | Evidence |
|---|---|
| Line hash, weights, closeness: bit-exact | `crates/core-diff/tests/closeness.rs`, `core-text` unit tests, golden values from the independent model: `python -I .claude/skills/golden-case/scripts/refmodel.py selftest` |
| Op stream (chunks 200 / 501 / 2000) | `crates/core-diff/tests/golden.rs`, `big_runs.rs`, `align_behaviour.rs` |
| Standard / None rows + review corpus | `crates/core-diff/tests/review_corpus.rs` (63 pairs, snapshots). `python -I tests/review/approve.py`: **57 cases cross-checked against the reference model, 0 problems** |
| Every corpus case under every align kind | `review_corpus.rs::every_review_case_is_sound_under_every_align_kind` (63 × 5: coverage, order, Same rows identical) |
| Myers minimal; Histogram, Patience valid | `exact_engines.rs`, `align_kinds.rs`, `props.rs`; differential against `imara-diff` in `tests/differential` |
| Folder compare per-item status | `crates/core-folder/tests/folder.rs`, `counters.rs`, `review_fixes.rs` (golden trees on `MemVfs`) |
| Sync: plan + disk effect | `crates/core-sync/tests/plan.rs` (preset table), `exec.rs`, `disk.rs` |
| Review corpus approved | Snapshots approved by the independent model (above). **Open: the owner's visual sign-off** of the rendered pages (`cargo run -p dsdev -- render <case> --html`) |

## 2. Every §9 safety rule has a passing test

| Rule | Tests (crates/core-sync/tests unless stated) |
|---|---|
| 9.1 re-validate before acting | `exec.rs::destination_changed_since_the_compare_asks_and_skip_leaves_it_untouched`, `retry_after_the_destination_is_restored_goes_through`, `a_vanished_source_and_an_appeared_destination_are_questions_too`; `review_fixes.rs::a_file_changed_after_the_compare_is_not_deleted`; `fileops.rs::copy_refuses_existing_destinations_unless_overwrite_and_revalidates` |
| 9.2 atomic replace | `exec.rs::a_failure_at_every_write_step_leaves_old_or_new_content_only`, `cancel_at_every_step_is_clean`, `pause_holds_the_job_and_cancel_stops_it_without_leftovers`; `disk.rs::killed_mid_copy_leaves_the_old_or_the_new_file` (child process killed mid-copy); `core-vfs/tests/conformance.rs` |
| 9.3 type-conflict ordering | `plan.rs::type_conflict_delete_runs_before_the_copy`, `type_conflict_without_a_delete_is_a_plan_error`; `exec.rs::mirror_replaces_a_folder_by_a_file_and_back`; `review_fixes.rs::type_conflicts_work_on_a_case_insensitive_target_and_leave_a_clean_tree` |
| 9.4 deletes under filters | `exec.rs::filtered_folder_delete_keeps_what_was_never_compared`, `a_folder_that_gained_a_file_after_the_compare_is_kept`; `fileops.rs::delete_items_deletes_post_order_and_stays_inside_what_it_listed` |
| 9.5 links never followed | `disk.rs::a_junction_is_never_traversed_and_its_target_stays_intact`; `plan.rs::links_are_skipped_rows`; `fileops.rs::links_are_not_copied_or_followed_by_file_operations`; `review_fixes.rs::a_link_inside_an_orphan_folder_is_kept_with_its_folder`; `core-vfs/tests/win32.rs` |
| 9.6 dry run everywhere | `plan.rs::describe_lists_the_exact_actions_for_a_dry_run`; `core-ffi/tests/folder_sync.rs` and the C# self test ("a plan writes nothing") |
| 9.7 overlapping roots | `plan.rs::nested_roots_refuse_planning`; `review_fixes.rs::file_operations_refuse_to_go_into_themselves`, `a_plan_cannot_run_against_another_compare`; `fileops.rs::copying_a_folder_into_itself_and_bad_names_are_refused_before_anything_is_written`; `core-ffi/tests/folder_sync.rs::nested_roots_refuse_to_plan` |
| 9.8 names the target cannot hold | `plan.rs::destination_names_are_validated_up_front`; `core-vfs` name-validation tests |
| Fault injection | `FaultVfs` in `exec.rs` (a failure at every write step, cancel at every step), `fileops.rs::a_failed_copy_in_a_move_keeps_the_source` |

## 3. Every §12.2 budget is met

`cargo xtask bench` on the dev machine (the table is printed by the command):

| Budget | Value |
|---|---|
| line indexing ≥ 1 GB/s | 8.84 GB/s |
| first 200 rows ≤ 100 ms (2 × 100 MB) | 91.7 ms |
| full alignment ≤ 1.5 s | 0.84 s |
| 50k × 50k all different ≤ 3 s, ≤ 300 MB, no stall of `get_rows` | `core-diff/benches/worst_case.rs`, its own process: 2.15 s, **peak process memory 17 MB**, the slowest read call during the alignment 0.05 ms. Also `core-diff/tests/session.rs::reads_never_stall_while_the_worst_case_aligns` |
| viewport pull ≤ 0.2 ms | 0.016 ms |
| folder scan ≥ 150k entries/s | 1.0 M entries/s on a real folder (100 000 files, `scan_real_disk`), 2.0 M on the in-memory file system |
| 1M nodes ≤ 160 MB | 136 MB as `Tree::memory_bytes()` (nodes and name arena); the process peak of that test was not measured |
| content compare ≥ 90% of disk read speed | Warm cache on the NVMe: 1.1 to 1.5 × the unbuffered read speed (Binary and CRC-32). **Cold cache on the SATA HDD** (`CONTENT_BENCH_COLD_DIR`, the cache emptied before each run, both files on the HDD): 0.90 (Binary) and 0.97 (CRC-32) of a serial unbuffered read. That run found that overlapping the two reads made the HDD compare 25% *slower* (0.75); it is now chosen by disk type (D54) |
| 1 GiB copy within 5% of robocopy | 0.71 × robocopy's time |

Content compare needed a change to reach the budget: both sides of a file of 8 MiB or more are now
read at the same time (it was 76% of the disk before). Pinned by
`core-folder/tests/folder.rs::big_files_use_the_overlapped_readers_with_the_same_results`.

The runner (`xtask bench`) now fails when a required budget is not reported, and finds the criterion data through `cargo metadata`.

The cold HDD run is an opt-in measurement (it writes a 40 GB scratch file to empty the cache) and is not in `cargo xtask bench`.

**Open:** the CI `bench` job (windows-latest) has not run. A GitHub runner is not the reference
machine (its disk and CPU are slower), so the absolute budgets may need a runner-specific
decision there. The 10% regression gate compares with the last run on `main` of the same
runner type and needs one run on `main` to have a baseline.

## 4. ABI 1.0 is frozen, documented and proven by the Avalonia test app

* `ds_abi_version()` is 1.0 (`crates/core-ffi/src/lib.rs`); `docs/abi.md` is the reference, with
  the event contract (decision D51) and the versioning policy.
* Layout of every struct is asserted in Rust and in `tests/c-smoke/smoke.c`;
  `cargo xtask bindings --check` (header and C# bindings are current); enum numbers are pinned by
  `core-ffi/tests/lifecycle.rs::enum_numbers_are_pinned`.
* Proven by `tools/diffsync-testapp` (Avalonia): `--selftest` passes (text, folder grid within frame
  budget, sync dry run and question handshake), and a NativeAOT publish launches.
* **Open:** the interactive manual UI script (E10-S2.2) needs a person at the screen; the tag
  `abi-1.0` needs a commit (E10-S3.1).

## 5. `cargo deny` is clean; no `unsafe` outside the allowed modules

* `cargo deny check`: advisories ok, bans ok, licenses ok, sources ok. One narrow exception:
  `cbindgen` (MPL-2.0) is allowed because it only runs inside `cargo xtask bindings` and is never
  linked or shipped (`deny.toml`; the owner should confirm).
* `#![forbid(unsafe_code)]` in core-base, core-diff, core-glob, core-folder and core-sync;
  `#![deny(unsafe_code)]` in core-text and core-vfs, with `allow` only in `core-text::mmap`,
  `core-text::ansi` (the `GetACP` declaration) and `core-vfs::local_win32`; core-ffi is the C
  boundary. Every `unsafe` block has a `// SAFETY:` comment.

## Other E11 acceptance evidence

| Item | Evidence |
|---|---|
| Fuzz targets (E11-S1) | `fuzz/fuzz_targets/{align_small_skew,text_decode,glob_patterns,options_json}.rs`; FFI random calls: `core-ffi/tests/random_calls.rs`, a seeded in-process generator instead of a libFuzzer target (a cdylib cannot link under libFuzzer; the `rlib` could, which would add coverage guidance: open). It asserts that no export returns `DS_E_INTERNAL` and that the handle count returns to its baseline, runs a real event callback in every other round, and covers text, folder, view, plan, job, file operations and the sweep. Campaign on the dev machine: 2 minutes per cargo-fuzz target, 0 crashes (1.45 M, 0.29 M, 1.36 M and 0.13 M executions); 800 000 random FFI calls, 0 failures after the fix below. The stricter FFI campaign found one real defect (out-of-range node numbers reached a worker thread: now `DS_E_INVALID_ARG`, `lifecycle.rs::node_numbers_outside_the_tree_are_invalid_arguments`). CI smoke job enabled. **Open: the 1 hour per target the backlog asks for was not run** |
| Decision ledger (E11-S4) | `cargo xtask ledger`: 59 rows, every one pinned by an existing test |
| README (E11-S5.1) | `cargo test -p readme-tests --doc` runs its Rust examples; the C# example is `DiffSync.Native.Tests::ReadmeExample`; the performance table has measured numbers |
| License (E11-S5.2) | **Open: the owner chooses; no `LICENSE` file yet** |

## Review of the whole tree (E11-S6.2) and what was done about it

The `plan-reviewer` pass over the workspace found no BLOCKER and four MAJOR items. Status:

| Finding | Status |
|---|---|
| MAJOR: `recompare` during `Loading` left the session `Failed` for good | **Fixed**: the sources are kept until loaded and the load restarts with the new options. `core-diff/tests/session.rs::recompare_while_loading_restarts_the_load` (fails without the fix) |
| MAJOR: D51 had no test that `ds_close` waits for a running callback and that callbacks never overlap | **Fixed**: `lifecycle.rs::close_waits_for_a_running_callback_and_callbacks_never_overlap` |
| MAJOR: the random-call test and the DoD claim were overstated | **Fixed**: stricter test (above), claim reworded |
| MAJOR: §12.2 figures need qualification | **Fixed**: real-disk scan added to the gate, footnotes above. Peak process memory and a cold-disk content compare stay unmeasured |
| MINOR: a panic in the overlapped compare's helper thread was not contained; the CRC branch hashed on after the other side failed | **Fixed**: explicit join maps a panic to an error; a stop flag ends the other side. Tests: read failures on either side of a big pair, a cancel in the middle |
| MINOR: `jobName` could leave `logDir`; logs could overwrite each other | **Fixed**: validated (`DS_E_INVALID_ARG`), `create_new` with a new sequence number on a clash |
| MINOR: `ds_init` had no cap on `worker_threads` | **Fixed**: above 1024 is `DS_E_INVALID_ARG`; documented that it only applies before first use |
| MINOR: `bench.rs` could pass without a budget; relative `target` path | **Fixed** |
| MINOR: `ds_maintenance_sweep` was promised by plan §9.2 but not exported | **Fixed**: `ds_maintenance_sweep` (+ C# `FileOps.MaintenanceSweep`); header and bindings regenerated |
| MINOR: a second `ds_plan_execute` started a second job; `ds_plan_set_op` raced with it | **Fixed**: the plan is claimed under its lock; `lifecycle.rs::a_plan_executes_once` |
| MINOR: README said ARM64 is tested in CI; abi.md missed log sink and lone-surrogate notes | **Fixed** |
| MINOR (§9.5): a folder replaced by a junction between compare and execute is not detected for a step below it | **Fixed (D53)**: every folder above a step's source and destination must still be a plain folder, else a `ChangedSinceCompare` question. `core-sync/tests/disk.rs::a_folder_swapped_for_a_junction_after_the_compare_stops_the_steps_below_it` (a real junction; fails without the check) |
| MINOR: `ds_view_set` applies sort and search before it validates the filter (a failed call is partly applied) | **Fixed**: every field is resolved before any is applied. `lifecycle.rs::a_failed_view_set_leaves_the_view_unchanged` |
| MINOR: on non-Windows `ds_folder_open` converts roots lossily and does not check that they are absolute | **Open, documented in `docs/abi.md`**; Windows is the primary target |
| E5-S4.2 non-Windows case-sensitivity probes | **Implemented (D52)**: `LocalStd` measures case sensitivity by a case-swapped lookup instead of assuming it from the OS. Tested on Windows only; it has not run on macOS or Linux |
| MINOR: a callback-initiated `ds_text_recompare` on its own handle may let the old worker append rows after the clear | **Open, not confirmed**; the generation bump makes a host discard such rows |
