# diffsync_core C ABI

The C ABI of `diffsync_core.dll` (crate `core-ffi`). The header `include/diffsync.h` (cbindgen) and
`bindings/csharp/NativeMethods.g.cs` (csbindgen) are **generated** by `cargo xtask bindings` and
checked in; CI runs `cargo xtask bindings --check`. This page explains the conventions behind
them. When this page and `plan.md` §10 disagree, `plan.md` wins.

ABI version: **1.2** (`ds_abi_version()` = `major << 16 | minor`). 1.0 was frozen at E10-S3 after the
Avalonia proof (`tools/diffsync-testapp`, `bindings/csharp`): see "Versioning policy" for what
may still change. 1.1 added the integration exports, 1.2 the editing and patch exports (both below).

## Conventions

| Topic | Rule |
|---|---|
| Calling convention | `extern "C"`. One convention on x64 and ARM64 Windows |
| Status | Every function returns `DsStatus` (`int32_t`): `0` = `DS_OK`, negative = error (table below). Results go through out-parameters |
| Last error | `ds_last_error(buf, cap, &needed)` copies the calling thread's last error message (UTF-8, no NUL). Set by the failing call on the same thread. It is **not** cleared by a success |
| Panics | Every export runs inside a panic barrier: a panic becomes `DS_E_INTERNAL` with the message in the last error. Nothing unwinds across the ABI. A worker panic puts the session into its failed state with a message |
| Handles | `DsHandle` = `uint64_t`, from a generation-checked registry. `0` is never valid. A closed, stale or foreign-kind handle returns `DS_E_INVALID_HANDLE`; it never crashes. One `ds_close(h)` closes any handle |
| User strings | Paths, names and line text are **UTF-16** views `{const uint16_t* ptr; size_t len}`. Not NUL-terminated. WTF-16 names stay lossless |
| Machine strings | Options and reports are **UTF-8 JSON** views `{const uint8_t* ptr; size_t len}`. An empty view means "defaults" |
| Output buffers | Caller-allocated, **two-call pattern**: call with a null pointer and capacity 0 to learn the needed size (`DS_E_BUFFER_TOO_SMALL` and the size in the `len`/`needed` out-parameter), allocate, call again. A call with too small a buffer fills what fits and still reports the full size |
| Core-allocated blobs | JSON reports come back as `DsBuf {ptr, len}`; free with `ds_buf_free` exactly once |
| Input structs | Start with a `uint32_t struct_size` (`DsInitOptions`), so they can grow; new fields go **at the end** |
| Enums and structs | **Never renumber an enum, never reorder a struct field.** Ranges are reserved for later values |
| Threads | Any function may be called from any thread, at any time. Read functions never block on a worker. Callbacks arrive on core worker threads only |
| Options | JSON with `"version": 1`; missing fields take defaults; unknown fields are logged as warnings and ignored; a wrong version is `DS_E_INVALID_ARG`. `ds_options_schema` returns a JSON Schema (decision D45) |

### Status codes

| Code | Name | Meaning |
|---|---|---|
| 0 | `DS_OK` | |
| -1 | `DS_E_INVALID_ARG` | A bad argument: null pointer, bad JSON, unknown enum value, wrong version, refused request (nested roots, copy into itself) |
| -2 | `DS_E_INVALID_HANDLE` | Closed, stale, zero or wrong-kind handle |
| -3 | `DS_E_CANCELLED` | The work was cancelled |
| -4 | `DS_E_NOT_FOUND` | No such line, node, difference, or question |
| -5 | `DS_E_ACCESS_DENIED` | |
| -6 | `DS_E_BUFFER_TOO_SMALL` | The needed size was written; call again |
| -7 | `DS_E_BUSY` | Not ready yet (compare still scanning, job still running) |
| -8 | `DS_E_INTERNAL` | A bug or a panic; the last error says what |
| -9 | `DS_E_ALREADY_EXISTS` | |
| -10 | `DS_E_DISK_FULL` | |
| -11 | `DS_E_CHANGED_SINCE_COMPARE` | A file changed after the compare (plan.md §9.1) |
| -12 | `DS_E_UNSUPPORTED` | |
| -13 | `DS_E_IO` | |
| -14 | `DS_E_ENCODING` | |
| -15 | `DS_E_INVALID_NAME` | A name the target cannot hold (plan.md §9.8) |
| -16 | `DS_E_PATH_TOO_LONG` | |
| -17 | `DS_E_SHARING_VIOLATION` | |

## Events

One callback type serves every session and job:

```c
typedef void (*DsEventFn)(void* user, DsHandle src, uint32_t kind, uint64_t a, uint64_t b);
```

| Kind | Value | `a` | `b` |
|---|---|---|---|
| `DS_EV_STATE` | 1 | text: state code (255 = not known yet, call `ds_text_status`); folder: 1 done / 2 failed / 3 cancelled; job: 2 finished | 0 |
| `DS_EV_ROWS` | 2 | rows published so far (text) | 0 |
| `DS_EV_PROGRESS` | 3 | done | total (folder: items; job: steps) |
| `DS_EV_NODE` | 4 | nodes updated | tree revision |
| `DS_EV_QUESTION` | 5 | question id | 0 changed since compare / 1 failed |
| `DS_EV_LOG` | 6 | reserved (not sent in 1.0) | 0 |

Contract (decision D51):

* Called on **core threads only**: never on the thread of an API call, even for an event that a
  call itself produces (for example the state change of `ds_text_recompare`): that one is
  delivered from a short-lived core thread.
* **Serialised and ordered.** Callbacks of one handle never run concurrently and arrive in the
  order the events were produced.
* Coalesced to **at most one event per kind per 16 ms** (60 Hz). Events that are held back go out
  with the next one. State changes, questions and the final event of a session or job are never
  held; `DS_EV_PROGRESS` and `DS_EV_NODE` of a running job may be.
* No lock of the core is held while the callback runs. The callback may call any API function,
  including `ds_close` of its own handle, but it must **return quickly**: post to the UI
  thread. A callback that takes more than 500 ms is reported to the log sink. A callback must
  **not throw or panic**: an exception escaping an `extern "C"` function ends the process.
* An event may be produced **before the open call returns** (the handle is not known yet). It is
  held and delivered from a core thread right after the handle is known, so the host still gets
  it; calling the matching `status` function once after opening is good practice anyway.
* **After `ds_close(h)` returns, no callback of `h` runs or starts**, and `user` is not used
  again: the host may free it. `ds_close` waits for a callback that is running on another thread
  (and does not wait for the caller's own callback).
* `DS_EV_LOG` is reserved and not sent in 1.0; use the log sink of `ds_init`.
* The log sink may be called on the thread of the API call that found the problem (a warning
  about that call's own arguments), unlike event callbacks.

## Library

| Function | Notes |
|---|---|
| `uint32_t ds_abi_version(void)` | `major << 16 \| minor` |
| `DsStatus ds_init(const DsInitOptions*)` | Optional. Worker threads of the shared pool, a log sink (`DsLogFn`), the default folder for execution logs. A second call returns `DS_E_BUSY`. `worker_threads` above 1024 is `DS_E_INVALID_ARG`; it only takes effect if the shared pool was not used yet. `log_fn` may be called from several core threads at once, and `log_user` must stay valid for the life of the process (a log sink cannot be detached) |
| `DsStatus ds_last_error(uint8_t* buf, size_t cap, size_t* needed)` | Two-call pattern, UTF-8 |
| `void ds_buf_free(DsBuf)` | For every `DsBuf` the core returned |
| `DsStatus ds_close(DsHandle)` | Closes any handle and stops its work: a running scan, session or job is cancelled (a copy stops at its next block and never leaves a half-written file). When it returns no callback of the handle runs or starts. A stale or closed handle is `DS_E_INVALID_HANDLE` |
| `DsStatus ds_options_schema(DsBuf*)` | JSON Schema of the option sets `text`, `folder`, `sync`, `exec`, `view` |

## Text compare

`ds_text_open(left, right, options_json, cb, user, &h)` starts a compare of two files and returns
at once; loading and alignment run on a worker. Rows appear progressively. Pull functions copy a
viewport (about 100 rows) and never wait for the worker.

| Function | Notes |
|---|---|
| `ds_text_open` | Paths are UTF-16 and must be absolute (a relative path would resolve against the process current directory: `DS_E_INVALID_ARG`). Loading errors show up as state 3 (failed) with the message in `ds_last_error` after `ds_text_status` |
| `ds_text_status(h, DsTextStatus*)` | State (0 loading, 1 aligning, 2 done, 3 failed, 4 cancelled, 5 binary), rows so far, `generation`, the nine counters |
| `ds_text_get_rows(h, first, count, DsTextRow*, &written)` | `count` rows from `first`; `written` may be less at the end |
| `ds_text_get_sections(h, first, count, DsSection*, &written)` | Runs of difference rows |
| `ds_text_get_line(h, side, line, uint16_t* buf, cap, &len, DsLineInfo*)` | `side` 0 left / 1 right. Two-call pattern. `info` may be null |
| `ds_text_get_intraline(h, row, DsRange* left, lcap, &ln, DsRange* right, rcap, &rn)` | Word-level highlights of a paired row, UTF-16 units. Two-call pattern for both sides |
| `ds_text_find_diff(h, from_row, dir, important_only, &row)` | `dir >= 0` next, `< 0` previous. `DS_E_NOT_FOUND` at the end |
| `ds_text_get_overview(h, uint8_t* buckets, n)` | `n` overview buckets, 0 = equal, higher = more severe |
| `ds_text_doc_info(h, side, DsDocInfo*)` | Encoding, BOM, endings, size. `DS_E_BUSY` until the file is loaded |
| `ds_text_recompare(h, options_json)` | New alignment with other options. `generation` increases first; the rows of the old alignment are dropped when the call starts and the new ones appear progressively, so a host that sees 0 rows and a new generation knows it is a new alignment |
| `ds_text_cancel(h)` | |

## Folder compare and views

`ds_folder_open(left_root, right_root, options_json, cb, user, &h)` scans on a core thread.
Until the compare is done, node and view functions return `DS_E_BUSY`; `ds_folder_status` shows
progress. A failed compare (the same folder twice, a missing root) has state 2 and the message in
`ds_last_error`. Nested roots compare with a warning and set `sync_blocked`.

| Function | Notes |
|---|---|
| `ds_folder_status(h, DsFolderStatus*)` | State, progress, node count, counters, `sync_blocked`, `revision` (increases whenever the tree changes) |
| `ds_folder_node_info(h, node, DsDirRow*)` | One node |
| `ds_folder_node_name(h, node, side, buf, cap, &len)` | `side` 0 left, 1 right, 2 display. UTF-16, two-call pattern |
| `ds_folder_report(h, DsBuf*)` | JSON: warnings, per-node errors, `syncBlocked` |
| `ds_folder_compare_content(h, nodes, n)` | Runs content compare in the background (the rules may have it off); `DS_EV_NODE` when done |
| `ds_folder_refresh(h, nodes, n)` | Re-stats nodes in the background; `DS_EV_NODE` when done |
| `ds_folder_cancel(h)` | |
| `ds_view_create(folder, view_json, &v)` | A flat list of visible nodes over a finished compare. Several views can share one compare |
| `ds_view_set(v, view_json)` | Filter, search, sort. Fields not given stay. `filter`: `all differences same orphans leftNewer rightNewer`; `sort.key`: `name size time status`; folders always come first |
| `ds_view_expand(v, node, expanded, recursive)` | |
| `ds_view_row_count(v, &count, &generation)` | `generation` changes whenever the rows may have changed (including after the tree changed) |
| `ds_view_get_rows(v, first, count, DsDirRow*, &written)` | |

## Sync, jobs and file operations

`ds_sync_plan(folder, rules_json, nodes, n, &plan)` builds a **preview** from a finished compare
(`n = 0`: everything). Nothing is written until `ds_plan_execute`. Nested roots refuse
(`DS_E_INVALID_ARG`). Prompts are data: `ds_plan_get_prompts` lists the nodes whose choice is
open, with **Leave** preselected; `ds_plan_set_op(plan, node, op)` chooses (0 leave, 1 prompt,
2 copy to left, 3 copy to right, 4 delete left, 5 delete right) and rebuilds the plan.
`ds_plan_report` returns JSON with the issues (invalid names, blocked copies), skipped links and
the dry-run lines.

`ds_plan_execute(plan, exec_json, cb, user, &job)` starts a job. Copy and delete behaviour
(safe copy, Recycle Bin, read-only handling) comes from the plan's rules; `exec_json` may set
`logDir`, `jobName`, `verify`, `useRecycleBin`. The compare tree is refreshed after every action
(`DS_EV_NODE`, `revision`).

**Questions.** When an action fails or a file is not what the compare saw (§9.1), the worker
posts a question (`DS_EV_QUESTION`) and waits. `ds_job_get_question` returns it;
`ds_job_answer(job, question_id, answer)` answers from **any thread**: 0 retry, 1 skip,
2 skip all, 3 abort. The id must be the open question's id (`DS_E_NOT_FOUND` otherwise), so a
late click never lands on the next question. A cancel while a question is open is a cancel, not
an abort.

| Function | Notes |
|---|---|
| `ds_plan_summary`, `ds_plan_get_actions`, `ds_plan_get_prompts`, `ds_plan_report`, `ds_plan_set_op` | Preview. Node ids are those of the folder compare |
| `ds_maintenance_sweep(root, delete, &report)` | Plan §9.2: lists the `*.dctmp~` temp files a crash left under the absolute folder `root` (links are never entered) and, with `delete != 0`, removes them. Synchronous and a whole-tree walk: call it off the UI thread. Report JSON `{found:[{path,bytes}], deleted, errors:[{path,message}]}`, freed with `ds_buf_free` |
| `ds_fileops_copy(items, dst, opts_json, cb, user, &job)` | `items` is a UTF-16 list of absolute paths; `dst` a folder. Options `overwrite`, `logDir`, `useRecycleBin`. A copy into itself, onto itself, or onto an existing name without `overwrite` is refused before anything runs |
| `ds_fileops_move`, `ds_fileops_delete` | Move is a rename inside one volume, copy then delete across volumes (the delete waits for a verified copy). Deletes remove a folder only when it is empty afterwards |
| `ds_job_status(job, DsJobStatus*)` | Progress; the counts when finished |
| `ds_job_get_question`, `ds_job_answer` | See above |
| `ds_job_pause`, `ds_job_resume`, `ds_job_cancel` | Cancel leaves no half-written file |
| `ds_job_result(job, DsBuf*)` | JSON result of a finished job (`DS_E_BUSY` while running) |

Closing a job's handle cancels it (asynchronously; `ds_close` itself returns at once and no further event is delivered).

## Exports beyond plan.md §10.2

`ds_folder_report`, `ds_plan_get_prompts` and `ds_plan_report` are additions to the surface of
`plan.md` §10.2 (JSON reports and the list of open prompts), and `ds_folder_node_info` returns a
`DsDirRow` (the plan's `DsNodeInfo` is the same layout). `ds_plan_set_op` is refused with
`DS_E_BUSY` once the plan was executed; the folder-level and plan-level locks are always taken in
the order compare, then plan.

Folder roots of `ds_folder_open` must be valid UTF-16 on Windows (an unpaired surrogate is `DS_E_INVALID_ARG`); on other systems a root is converted lossily and is not checked for being absolute.

Paths in file operations (`ds_fileops_*`) must be absolute and must be valid UTF-16: an unpaired
surrogate is `DS_E_INVALID_ARG` (a lossy conversion would name a different file). A `logDir`
that is empty means "none given" (the folder from `ds_init`, if any); a relative one is
`DS_E_INVALID_ARG`.

## Integration exports (ABI 1.1, backlog E11.5)

Added in 1.1; nothing of 1.0 changed. Walk-through of the callers: `docs/integration.md`. Every JSON
report is UTF-8 in a `DsBuf` freed with `ds_buf_free`. A diff that finds a difference, or cannot read
a file, is still a successful *call*: the report carries the GNU exit code and the text for standard
error. A malformed request is `DS_E_INVALID_ARG`.

| Function | Notes |
|---|---|
| `ds_diff_run(request, &report)` | Runs a `diff` / `git diff` command line on the local file system. Request `{"args":["-ru","a","b"], "dialect":"gnu"\|"git", "ignore":"<pattern list>", "ignoreLockFiles":false, "maxFileBytes":536870912}`. Report `{"stdout","stderr","exitCode","json","ignored"}`; `json` is a JSON document in a string: `{"exitCode","different","ignored","summary":{"filesChanged","insertions","deletions","sameFiles"},"entries":[...]}` where an entry is `{"kind":"file","path","change":"modified"\|"added"\|"deleted","binary","tooLarge","hunks":[{"oldFirst","oldCount","newFirst","newCount","func","lines":[{"op":"+"\|"-"\|" ","text","eol"}]}],"additions","deletions",...}`, `{"kind":"onlyIn",...}`, `{"kind":"commonDir"}`, `{"kind":"typeConflict"}` or `{"kind":"error"}`. Supported flags: `docs/integration.md` §1. Synchronous: call off the UI thread |
| `ds_diff_texts(request, &report)` | The same for two texts: `{"args":["-u"], "left":"..", "right":"..", "leftName":"old", "rightName":"new", "dialect":"gnu"}` (`args` are flags only) |
| `ds_tool_parse_args(args_json, &report)` | How another program started the tool: a JSON array of arguments in, `{"invocation":{"kind":"compare"\|"merge"\|"gitExternalDiff"\|"selectLeft"\|"compareSelected","value":{...}},"unknown":[...]}` out. A command line the protocol does not understand is `DS_E_INVALID_ARG` |
| `ds_tool_default_ignore(&text)` | `*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved` |
| `ds_tool_is_ignored(patterns, path, &flag)` | Whether the path matches the list (comma, semicolon or line separated; case ignored on Windows; matches on the full path) |
| `ds_merge3(request, &report)` | **Stub.** `{"base","local","remote"}` texts; `{"outcome":"clean","text","reason"}` when only one side changed or both are equal, else `{"outcome":"conflict","reason"}` |
| `ds_merge_files(request, &report)` | The merge-tool contract on files: `{"local","remote","base","output"}`; writes `output` and reports `{"exitCode":0}` only for a clean merge; `1` not merged (the output is left alone); `2` trouble |
| `ds_shell_selection_set(path)` / `_get(&json)` / `_clear()` | The Explorer "left side": an absolute path in, `{"left","chosenAt"}` out (an empty buffer when none; a side older than a day or no longer existing is none). Stored in `%LOCALAPPDATA%\DiffSync\selection.json` (`DIFFSYNC_STATE_DIR` moves it) |
| `ds_volume_info(path, &DsVolumeInfo)` | Free and total bytes of the volume (Windows; `DS_E_UNSUPPORTED` elsewhere) |
| `ds_text_get_folds(h, context, important_only, first, count, out, &written, &total)` | The runs of rows the Context view hides (rows farther than `context` from a difference); `total` is the number of folds |
| `ds_text_get_section_kinds(h, first, count, out_u8, &written)` | Per section: 0 insert, 1 delete, 2 replace |
| `ds_text_find_section(h, from_row, dir, kind, &row)` | The next (`dir` 0) or previous (1) section of that kind; `DS_E_NOT_FOUND` at the ends |
| `ds_text_find(h, query_utf8, flags, from_row, dir, &DsFindHit)` | `flags`: 1 case sensitive, 2 whole word, 4 regular expression, 8 skip left, 16 skip right; `DS_E_NOT_FOUND` when there is no match |

New option keys (additive): text options `eol` (`"ignore"` default, `"strict"`, `"crlfAsLf"`); exec
and file-operation options `copyRetries` (default 2) and `copyRetryWaitMs` (default 1000).

## Editing, patches and folder items (ABI 1.2, backlog E11.5-S14, S16, S17)

Added in 1.2; nothing of 1.0 or 1.1 changed. All JSON is UTF-8; reports are in a `DsBuf` freed with
`ds_buf_free`. Walk-through: `docs/integration.md` §6 and §7.

| Function | Notes |
|---|---|
| `ds_text_edit(session, request, &report)` | One edit of the text under the same handle. `op`: `copyRows {from, firstRow, lastRow}`, `copySection {from, section}`, `copyAll {from, withFormat}`, `replaceLines {side, firstLine, removeLines, text, refresh}`, `replaceRange {side, startLine, startColumn, endLine, endColumn, text, refresh}` (0-based lines, UTF-16 columns: what a piece-tree buffer reports), `applyEdits {side, edits[], refresh}` (a batch against the current text, one undo step), `setText {side, text, refresh}`, `refresh`, `undo {side}`, `redo {side}`, `convert {side, eol: crlf\|lf\|cr, encoding}`, `reinterpret {side, encoding, discardChanges}`, `state`. `side`/`from` are `left` or `right`. Report `{changed, removed, inserted, stale, generation, left, right}` with per side `{lines, dirty, canUndo, canRedo, encoding, bom, endingCounts, mixedEndings, defaultEol, lossy, revision}`. After an edit the compare restarts on the new text (status `Aligning`, `generation` changes): pull rows again as after a recompare. `refresh: false` skips the restart (`stale: true`); copy operations bring the compare up to date themselves. A text the encoding cannot hold, a lossily read file and a binary side are `DS_E_ENCODING` / `DS_E_UNSUPPORTED` |
| `ds_text_save(session, request, &report)` | `{side, path?, mode: "atomic"\|"inPlace", backupSuffix?, overwriteChanged, writeReadonly, overwriteExisting, eol?, encoding?}`. Writes the side in its encoding, byte order mark and line endings through a temporary file and a rename (D63); `DS_E_CHANGED_SINCE_COMPARE` when the file changed after it was read, `DS_E_ALREADY_EXISTS` for a Save As over another file. Report `{path, bytes, backup, size, mtime, replaced, state}` |
| `ds_patch_apply(request, &report)` | Applies a unified or git mailbox patch, SEARCH/REPLACE blocks, JSON Patch or a byte script to files under `root` or to a `text`: `{patch, format: auto\|unified\|searchReplace\|jsonPatch\|byteScript, root, target, text, strip, reverse, dryRun, fuzz, ignoreWhitespace, partial, maxFileBytes}`. All or nothing unless `partial`. A patch that does not apply is a successful call with `ok: false` and the reason per hunk, block or operation |
| `ds_json_patch(request, &report)` | `{"op":"diff","left","right"}` → `{"ok":true,"patch":[...]}`; `{"op":"apply","doc","patch"}` → `{"ok":true,"doc"}` or `{"ok":false,"error","index"}` |
| `ds_bytes_diff(request, &report)` | `{left, right, format: fc\|cmp\|script, maxRuns}` → `{identical, leftLen, rightLen, differingBytes, runs[{offset,left,right}], truncated, text}` |
| `ds_folder_set_ignored(folder, nodes, n, ignored)` | Marks items (and what is below them) as ignored, or clears the mark; views hide them unless `showIgnored`, a sync plan skips them |
| `ds_fileops_rename(item, new_name, options, cb, user, &job)` | Renames a file or folder as a job; the name is validated (§9.8), must not exist, a change of case alone is allowed where names ignore case |

New option keys (additive): view `filter` values `noOrphans differencesNoOrphans leftOrphans
rightOrphans leftNewerOrphans rightNewerOrphans` (with the older `all differences same orphans
leftNewer rightNewer` these are every mode of the reference products' filter menus), view
`ignoreUnimportant` and `showIgnored`; exec and file-operation options `durableCopy` (default true:
flush before the rename, D65) and `logRequired` (default true: no log, no job, D66); folder option
`allowUncheckedRoots` (default false, D68). Argument forms of the diff exports: `--search-replace`,
`--format-patch` with `--subject`, `--author`, `--patch-date`, `--message`.

## Struct layouts

All structs are `repr(C)`; sizes are asserted in the crate and again in `tests/c-smoke`.

| Struct | Size | Notes |
|---|---|---|
| `DsTextRow` | 16 | `left_line`, `right_line` (`UINT32_MAX` = absent), `kind` u8 (RowKind), `flags` u8 (bit 0 section start, bit 1 important), `reserved` u16, `section` u32 (`UINT32_MAX` for equal rows) |
| `DsRange` | 12 | `start`, `len` (UTF-16 units), `kind` u8 (1 important), 3 pad |
| `DsSection` | 12 | `first_row`, `row_count`, `important` u8, 3 pad |
| `DsLineInfo` | 8 | `ending` u8 (0 none, 1 CRLF, 2 LF, 3 CR, 255 unknown), 3 pad, `utf16_len` |
| `DsSideInfo` | 32 | `size` u64, `mtime` i64 (FILETIME ticks), `ctime` i64 (0 in v1), `attrs` u32, `present` u8, `kind` u8 (0 file, 1 folder, 2 link), `reserved` u16 |
| `DsDirRow` | 88 | `node`, `parent`, `depth` u16, `status` u8, `flags` u8, `child_count`, `left` (offset 16), `right` (offset 48), `diff_count` (80), `orphan_count` (84) |
| `DsPlanAction` | 16 | `kind` u8, `is_folder` u8, `flags` u16 (bit 0 delete only if empty, bit 1 frees a name), `node`, `bytes` u64 |
| `DsFold` | 8 | `first_row`, `row_count` |
| `DsFindHit` | 20 | `row`, `line`, `start`, `end` (UTF-16 units), `side` u8 (0 left, 1 right), 3 pad |
| `DsVolumeInfo` | 16 | `free_bytes` u64, `total_bytes` u64 |
| `DsQuestion` | 16 | `id` u64, `kind` u32 (0 changed since compare, 1 failed), `step` |

`DsTextStatus`, `DsDocInfo`, `DsFolderStatus`, `DsPlanSummary`, `DsJobStatus` are plain counters
documented in `include/diffsync.h`.

## Enum values (never renumbered)

* `RowKind`: `Same 0, Unimportant 1, Different 2, LeftOnly 3, RightOnly 4,
  UnimportantLeftOnly 5, UnimportantRightOnly 6` (reserved 16..31 for moved blocks).
* Node `status`: `Same 0, UnimportantDiff 1, Different 2, LeftNewer 3, RightNewer 4,
  LeftOrphan 5, RightOrphan 6, Error 7`.
* Node `flags`: `1 pending content, 2 content compared, 4 type conflict, 8 duplicate key,
  16 offline, 32 link, 64 filtered children, 128 gone`.
* Plan action `kind`: `0 create folder left, 1 create folder right, 2 copy to left,
  3 copy to right, 4 delete left, 5 delete right`.
* Sync operation (`ds_plan_set_op`): `0 leave, 1 prompt, 2 copy to left, 3 copy to right,
  4 delete left, 5 delete right` (6.. reserved for moves).
* Job state: `0 running, 1 paused, 2 finished`. Folder state: `0 scanning, 1 done, 2 failed,
  3 cancelled`. Text state: see `ds_text_status`.
* `Stage` (`DsFolderStatus.stage`): `Idle 0, Loading 1, Indexing 2, Aligning 3, Scanning 4,
  Comparing 5, Planning 6, Copying 7, Deleting 8, Verifying 9, Done 10`.

## Ownership and threading summary

* The host owns every buffer it passes in and every `DsHandle` until it calls `ds_close`.
* The core owns `DsBuf` results until the host calls `ds_buf_free`.
* Pointer arguments are only read or written during the call. The core never keeps a pointer
  past the call, except `user`, which it keeps until the handle is closed.
* `DsStr16`/`DsStr8` views need not stay alive after the call returns.
* Read functions (`get_rows`, `status`, `node_info`...) never block on a worker. `ds_job_*`
  functions never block. Only the host decides when to wait.

## Versioning policy

* The ABI version is `major << 16 | minor`. **Additive** changes (a new function, a new struct type, a new enum value, a new status code, a new JSON option) bump the
  minor. A **breaking** change (removing or renumbering anything, reordering fields, changing a
  signature) bumps the major and is avoided.
* Node numbers (`ds_folder_refresh`, `ds_folder_compare_content`, the selection of `ds_sync_plan`) must exist in the tree: a number outside it is `DS_E_INVALID_ARG`.
* `jobName` is part of a log file name: no `/`, `\`, `:` or `..`, at most 100 bytes (`DS_E_INVALID_ARG`).
* **Output structs have a fixed size** within one major version (their sizes and offsets are asserted in the crate and in the C smoke test): a field is never appended to one. A new fact needs a new function or a new struct type. **Input structs** start with `struct_size`, so they can grow; the core refuses a `struct_size` smaller than the struct it knows and reads only the fields inside `struct_size` from a later minor on.
* The 1.0 header is tagged `abi-1.0` when the repository is committed (E10-S3). From then on
  `cargo xtask bindings --check` and the C smoke test (sizes and offsets of every struct) guard
  the layout, and the `lifecycle` test pins the enum numbers.
