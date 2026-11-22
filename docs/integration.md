# Integrating the core: command line, source control, Explorer, agents (backlog E11.5)

The core does the work; a program on top of it (the owner's CLI, an MCP server, the DeltaForge UI) is
a thin layer. This page says what the layer calls, with the commands that other programs use to start
it.

## 1. What a command-line program calls

A command line has two shapes. `core-tools` (Rust) and the ABI (C, C#) understand both.

```rust
let args: Vec<String> = std::env::args().skip(1).collect();

// (a) "diff" mode: the flags of `diff` or `git diff`, two files or folders
let req = core_tools::parse_diff_args(&args, core_tools::Dialect::Gnu)?; // or Dialect::Git
let out = core_tools::run(&req, &core_tools::RunOptions::default());
print!("{}", out.stdout);
eprint!("{}", out.stderr);
std::process::exit(out.exit_code);

// (b) "tool" mode: how SourceTree, git difftool/mergetool, GIT_EXTERNAL_DIFF and Explorer start you
match core_tools::parse_invocation(&args)?.invocation {
    Invocation::Compare(c)          => { /* open the compare window, or print a diff */ }
    Invocation::Merge(m)            => { let r = core_tools::merge_files(..); std::process::exit(r.exit_code) }
    Invocation::GitExternalDiff(g)  => { /* diff g.old_file against g.new_file in git style */ }
    Invocation::SelectLeft(p)       => { /* the shell extension's "Select left side" */ }
    Invocation::CompareSelected(p)  => { /* compare the remembered left side with p */ }
}
```

Suggested subcommands, so the two shapes never clash: `diffsync diff ...` (mode a),
`diffsync compare L R`, `diffsync merge L R B -o M`, `diffsync git-extdiff <7 args>` (mode b).

From C# the same calls are `Tools.DiffRun(json)`, `Tools.ParseArgs(json)` (ABI 1.1, `docs/abi.md`).

### `diff` mode: what is supported

| Command | Output |
|---|---|
| `diff a b` | normal format (`2c2`, `< x`, `> y`) |
| `diff -u a b`, `-U N`, `--unified=N` | unified, N lines of context (default 3) |
| `diff -c a b`, `-C N` | context format |
| `diff -r[uN] dir1 dir2` | recursive, `diff -ru dir1/x dir2/x` lines, `Only in d1: name` |
| `diff -q`, `-s` | `Files a and b differ`; `Files a and b are identical` |
| `-i -w -b -Z -B --strip-trailing-cr` | ignore case, all whitespace, whitespace amount, trailing whitespace, blank lines, CR before LF |
| `-x PAT`, `--exclude`, `-L LABEL` | exclude files (glob syntax), header labels |
| `--minimal`, `--patience`, `--histogram`, `--diff-algorithm=` | the aligner |
| `git diff --no-index a b` (`Dialect::Git`) | `diff --git a/x b/x`, `new file mode`, `deleted file mode`, `/dev/null`, `@@ ... @@ function` |
| `--stat`, `--numstat`, `--shortstat`, `--name-only`, `--name-status` | git's summaries |
| `--exit-code`, `--quiet`, `--no-prefix`, `--src-prefix`, `--dst-prefix` | git's switches |

Exit codes: GNU `0` same, `1` different, `2` trouble. Git style exits 0 unless `--exit-code` or
`--quiet` is given. The output equals GNU diff 3.12 and git 2.52 on the shapes pinned by
`core-folder/tests/udiff.rs` and `core-tools/tests/cli_equivalence.rs` (decision D56); where a
minimal edit script is not unique the core may choose another equally short one.

Line endings count: a CRLF file and an LF file differ, a missing final newline is a change and prints
`\ No newline at end of file` (decision D55). `--strip-trailing-cr` makes CRLF and LF equal.

## 2. SourceTree

*Tools, Options, Diff*:

| Setting | Value |
|---|---|
| External Diff Tool | Custom |
| Diff Command | `C:\Tools\DiffSync\diffsync.exe` |
| Arguments | `compare --ignore=*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved "$LOCAL" "$REMOTE"` |
| Merge Tool | Custom |
| Merge Command | `C:\Tools\DiffSync\diffsync.exe` |
| Arguments | `merge "$LOCAL" "$REMOTE" "$BASE" -o "$MERGED"` |

`--ignore` takes a list separated by commas, semicolons or line breaks; a file pair whose name
matches is skipped (nothing shown, exit 0). The built-in list for the four patterns above is
`core_glob::LOCK_FILE_PATTERNS`, `ds_tool_default_ignore`. Matching ignores case on Windows and
works on the full path (`D:\repo\web\package-lock.json` matches `*package-lock.json`).

**The three-way merge is a stub.** `merge_files` / `ds_merge_files` writes the output and exits 0
only when nothing needs a decision (both sides equal, or only one side changed). Any real conflict
exits 1 and leaves `$MERGED` alone, so SourceTree reports "not merged" instead of getting a wrong
file. The merge engine is backlog E11.5-S14.4.

## 3. git

```
git config --global diff.tool diffsync
git config --global difftool.diffsync.cmd '"C:/Tools/DiffSync/diffsync.exe" compare "$LOCAL" "$REMOTE"'
git config --global merge.tool diffsync
git config --global mergetool.diffsync.cmd '"C:/Tools/DiffSync/diffsync.exe" merge "$LOCAL" "$REMOTE" "$BASE" -o "$MERGED"'
git config --global mergetool.diffsync.trustExitCode true

# make `git diff` itself print the core's output (7 arguments per file, GIT_EXTERNAL_DIFF)
git config --global diff.external "C:/Tools/DiffSync/diffsync.exe git-extdiff"
```

`parse_invocation` recognises the seven (and nine, for renames) arguments by their shape: two hashes
(or zeros) and two six-digit modes (or `.`).

## 4. Windows Explorer

Right-click a file or folder: **Select left side**; then right-click another: **Compare with
"<left>"** (shown only for the same kind, and not for the item itself). The handler is
`diffsync_shell.dll` (`crates/shell-ext`, an `IExplorerCommand`), the decisions are
`core_tools::shell`, the remembered side is `core_tools::SelectionStore` (a small file in
`%LOCALAPPDATA%\DiffSync`, also readable through `ds_shell_selection_get`).

```
scripts\build-core.cmd -Shell                                  # builds dist\x64\diffsync_shell.dll
scripts\shell\register-shell.ps1 -DllPath dist\x64\diffsync_shell.dll `
    -Command '"C:\Tools\DeltaForge.exe" "%L" "%R"'             # this user only, no administrator
scripts\shell\register-shell.ps1 -Unregister -RestartExplorer  # removes everything it added
```

* The classic menu (Windows 10, and Windows 11 under "Show more options" or Shift+F10) is tested:
  the registration and removal scripts were run, and the COM objects are driven by an automated test
  through real `IShellItemArray`s.
* The **Windows 11 top-level menu** needs a package identity. `shell\AppxManifest.xml` and
  `scripts\shell\register-modern-menu.ps1` build a sparse package for it. They are written from
  Microsoft's documentation and **have not been run**; they need Windows 11, the Windows SDK and a
  signing certificate (a self-signed one for development, a real one to ship).
* The command line template (`%L` left, `%R` right) comes from the environment variable
  `DIFFSYNC_SHELL_COMMAND`, then `HKCU\Software\DiffSync\Shell\Command`, then `DeltaForge.exe` next to
  the DLL. A failure shows a message box instead of crashing Explorer.

## 5. Agents and MCP servers

A model reads unified diffs best, and the core produces exactly that. An MCP server needs no new core
code; each tool is one call:

| Tool | Call | Returns |
|---|---|---|
| `diff_texts(left, right, left_name, right_name, context)` | `ds_diff_texts` with `args: ["-U<context>"]` | unified diff text, `exitCode` |
| `diff_files(a, b, context)` | `ds_diff_run` with `args: ["-u", a, b]` | unified diff, `exitCode`, `json` |
| `diff_folders(a, b, summary_only)` | `ds_diff_run` with `args: ["-r", "-q", a, b]` or `["--no-index", "--stat", a, b]` (git dialect) | the list or the stat table |
| `changed_files(a, b)` | `ds_diff_run`, git dialect, `["--no-index", "--name-status", a, b]` | `M\tpath` lines |

Advice that keeps answers small and exact:

* Ask for `--stat` or `--name-status` first on folders, then the unified diff of the files that
  matter; set `ignoreLockFiles: true` so lock files do not drown the answer.
* Use `-U` with a small context for big files; use `--ignore-blank-lines` / `-w` only when asked, since
  they change what counts as different.
* The `json` field of the report is the same comparison as data: `entries[].hunks[].lines[]` with
  `op` (`+`, `-`, space), `text` and `eol`, plus `summary` counts. Use it to build patches or to
  quote a line number: the hunk header gives `oldFirst` and `newFirst` (0-based).
* Every call is synchronous and reads the files given. `maxFileBytes` (default 512 MiB) turns a file
  above it into "too large" instead of reading it. Binary files are reported as
  `Binary files a and b differ`, never dumped.
* Never feed colour or pager output (`delta`, `difftastic`, `hunk`) to a model: those tools are for
  people. This engine prints plain text only.

## 6. Patch formats: writing, reading and applying

The core writes and reads these; the same functions serve users, a command-line wrapper, the UI and
agents. What is worth having and what is not (decisions D58–D61):

| Format | Write | Read and apply | Where |
|---|---|---|---|
| Unified diff (`diff -u`, `git diff`) | yes | yes (`patch`/`git apply` rules: offset, fuzz, reverse, create, delete, rename) | `ds_diff_run`, `ds_patch_apply` |
| Context (`-c`) and normal diffs | yes | no (unified is what tools and models send) | `ds_diff_run` |
| Git mailbox (`git format-patch`) | yes (`--format-patch`, accepted by `git am`) | yes (message, author, date; several mails) | `ds_diff_run`, `ds_patch_apply` |
| SEARCH/REPLACE blocks | yes (`--search-replace`, grown until unique) | yes (exact, then white space, then indentation; ambiguous refused) | same |
| JSON Patch (RFC 6902) | yes | yes (atomic) | `ds_json_patch`, `ds_patch_apply` |
| Byte offset report and script (`fc /b`, `cmp -l`, `00001A4F:74->75`) | yes | yes (checks the old byte) | `ds_bytes_diff`, `ds_patch_apply` |
| Combined diff (`diff --cc`) | later (needs the merge engine) | later | backlog S14.4 |
| AST / structural diff (difftastic) | later (needs tree-sitter, E13-S4) | no | its output is made for people |
| VCDIFF / xdelta, bsdiff, zstd `--patch-from`, release manifests | not a compare feature: binary deltas for updates (E13-S7), the manifest and its signature are the update system's | no | backlog |
| IPS, byte-pattern (AOB) patchers | no | no | no verification of what is patched; not a compare or sync feature |

Applying is the §9 rules for patches: everything is computed in memory first and nothing is written
if any hunk fails (unless `partial`), a path in a patch cannot leave the root or pass a link, a file
is replaced through a temporary file after checking that it is still what was read, and `dryRun`
reports without writing. `strip` defaults to 1 for `a/` `b/` names. For a model: ask for
`--search-replace` or `--stat` first, apply with `dryRun: true`, read `files[].blocks[]` /
`hunks[]` for what did not fit, then apply for real.

## 7. Editing: copy arrows, a piece-tree text box, saving

A text compare is editable through `ds_text_edit` / `ds_text_save` (`docs/abi.md`). The model is one
list of lines per side with its own line endings, an undo history and the file's encoding and BOM.

* **Copy arrows.** File: `copyAll`. Block: `copySection` (the section as numbered by
  `ds_text_get_sections`). Line or selection: `copyRows` over the rows. A row with a gap on the source
  side deletes on the target, as the gap arrow does in Beyond Compare. Binary or whole-file copies are
  file operations (`ds_fileops_copy`), which are safe copies; a binary side is not edited as text.
* **A host that owns its buffer** (Avalonia's editor on a piece tree, VS Code's model): send each change
  as `replaceRange` (0-based lines, UTF-16 columns) or a batch as `applyEdits`, with `refresh: false`
  while the user types, and `{"op":"refresh"}` when typing pauses (a few hundred milliseconds). If the
  host and the core ever disagree, `setText` sends the whole buffer. The core never needs the host's
  piece tree, only the edits, and the rows it returns are for the text as of the last refresh.
* **Encoding, BOM and line endings.** `convert` changes how the text is written (Notepad++'s "Convert
  to"); `reinterpret` reads the same bytes as another encoding (its "Encode in"). Text a code page
  cannot hold is refused with the line and column, never replaced; a file that had malformed bytes is
  read-only until converted. Mixed line endings are reported (`mixedEndings`) and fixed by `convert`
  with `eol`.
* **Saving** is a temporary file, a flush and a rename after a check that the file is unchanged since it
  was read; `backupSuffix` keeps the old file; `mode: "inPlace"` keeps alternate data streams and hard
  links. A refused save writes nothing and says why.
* **Not yet:** three-way merge (`ds_merge3` is a stub), re-aligning only the edited region (every
  refresh re-compares the whole text, so a host should refresh when typing pauses), tree-sitter
  highlighting.

Recommended options for a UI that syncs important files: `useRecycleBin: true`, `verify: true` for
what matters, keep `durableCopy` and `logRequired` on, show the plan before executing, and say
"content equal" only for content compare (the default compare is size and time).

Threading: events arrive on core threads. In Avalonia pass `post: Dispatcher.UIThread.Post` and keep
the handler to "request a redraw"; do not call back into the same session from the handler.

## 8. Building

`scripts\build-core.cmd` (or `scripts\build-core.ps1`) builds `diffsync_core.dll` with a static C
runtime, regenerates the header and the C# bindings, and collects the DLL, the import library, the
PDB, `diffsync.h`, `NativeMethods.g.cs` and a `SHA256SUMS.txt` in `dist\`. `-Arch all` adds ARM64,
`-Shell` the Explorer extension, `-Test` runs the tests first, `-CheckBindings` is what CI uses.
