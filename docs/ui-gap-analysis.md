# UI readiness: what Beyond Compare, Araxis Merge and WinDiff show, and what the core offers (backlog E11.5-S9)

Source: the screenshots in `reference_source/UI` (Beyond Compare 5 text and folder compare and its
View, Search, Edit, File and Session menus, the Session Settings dialogs and the Home screen; Araxis
Merge text comparison; WinDiff). Each line says whether the core has it, where, or where it is
scheduled. "Core" means a call a UI makes; drawing and window behaviour are the UI's.

## Text compare

| Seen in the UI | Core | Call or plan |
|---|---|---|
| Two panes with aligned rows, gaps for lines missing on a side | yes | `ds_text_get_rows`, row kinds Same / Unimportant / Different / LeftOnly / RightOnly |
| Colours: important difference, minor ("misc") difference, hatched gap | yes | `RowKind` |
| Word- and character-level highlights inside a changed line (Araxis, BC) | yes | `ds_text_get_intraline` |
| Overview strip of all differences (minimap) | yes | `ds_text_get_overview` |
| Difference section arrows in the gutter; "23 difference section(s)" | yes | `ds_text_get_sections`, `ds_text_status` counters |
| Next / previous difference, next / previous difference section | yes | `ds_text_find_diff` |
| **Next / previous replacement** (section that changes lines, as against insert-only / delete-only) | **added** | `ds_text_get_section_kinds`, `ds_text_find_section` (ABI 1.1) |
| View filter: Show All / Differences / Same / Context / Ignore unimportant | yes | the host filters rows by kind; `Unimportant` rows are the "minor" ones |
| **Context view with "36 filtered lines" folds** | **added** | `ds_text_get_folds` (ABI 1.1) |
| File header: path, modified time, size in bytes, format, encoding, line ending | yes | `ds_text_doc_info` (encoding, BOM, line-ending counts, size); times from the file system |
| Status bar: line and column, "Load time", counts of removals, insertions, changes | yes | `ds_text_status` (counters, rows); the stopwatch is the host's |
| Session settings: whitespace modes, ignore case | yes | `TextOptions` (`whitespace`, `ignoreCase`) |
| Session settings: unimportant text (regexes) | yes | `TextOptions.unimportant` |
| Session settings: "Orphan lines are always important" | yes | `orphansAlwaysSig` |
| Session settings: **"Compare line endings (PC/Mac/Unix)"** | **added** | `TextOptions.eol` (`ignore`, `strict`, `crlfAsLf`), decision D55 |
| Session settings: alignment Unaligned / Standard / Myers / Patience; "Never align differences"; skew tolerance; "Use closeness matching" | yes | `alignment`, `neverAlignDiffs`, `skewTolerance`, `useCloseness` (plus Histogram) |
| Session settings: **Replacements** (regex rewrite before comparing) | no | E11.5-S13.3 |
| Recompare / Reload / Swap sides | yes | `ds_text_recompare`, reopen; swap is the host exchanging the two inputs |
| Find, Find next / previous, Go to line | **added** (find) | `ds_text_find` (case, whole word, regex, side); go to line is a host scroll |
| Bookmarks | host | UI state |
| Compare selection to clipboard, Open clipboard | yes | texts as input (`DocSpec::Text`; `ds_diff_texts` for patches) |
| **Text Compare Report** (HTML or text) | partly | unified, context and normal output exist (`ds_diff_run`, `ds_diff_texts`); HTML side-by-side: E11.5-S13.5 |
| Syntax highlighting, visible whitespace, line numbers, word wrap, rulers | host | the core gives text and ranges; highlighting needs a grammar (E12-S4) |
| **Copy to right/left, copy line, edit in place, undo/redo, save, save as, convert file (encoding, line ending), indent** | **no** | **E11.5-S14 (E12-S1)**: the largest gap; every screenshot has merge arrows. The core compares and reports; it does not yet change or save a text |
| **Align With… (F7)**: pin two lines as a pair | no | E11.5-S13.4 |
| Merge to common ancestor, Text Merge (three panes) | stub | `ds_merge3` decides only the trivial cases; the engine is E11.5-S14.4 |
| Isolate difference | no | with S14 (needs editing) |

## Folder compare

| Seen in the UI | Core | Call or plan |
|---|---|---|
| Tree with Name, Size, Modified of both sides, folders first | yes | `ds_view_get_rows`, `DsDirRow` (both sides' size, time, attributes, status) |
| Row colours and the `=` `≠` `≈` marks; orphans in red | yes | `status` of the row, flags |
| Show All / Differences / Same / Structure / Minor | yes | `ds_view_set` filter (`all`, `differences`, orphans, ...) |
| Expand / collapse, expand all | yes | `ds_view_expand` |
| Sort by column, search | yes | `ds_view_set` (`sort`, `search`) |
| **Filter bar** with patterns like `-*.aps;-*.ncb;-*.suo` | yes | folder rules `includeFiles`, `excludeFiles`, `excludeFolders`; the syntax is the glob dialect of `core-glob` |
| Background content comparison, "completed in 0.17 seconds" | yes | `ds_folder_compare_content`, events `DS_EV_PROGRESS`, `DS_EV_NODE`; the log line is the host's |
| Copy left / right, delete, move, sync dialog and preview | yes | `ds_sync_plan`, `ds_plan_set_op`, `ds_plan_execute`, file operations |
| Refresh, Stop | yes | `ds_folder_refresh`, `ds_folder_cancel` |
| Status bar "1 file(s) selected, 8.3 KB", "1.5 TB free on D:" | yes / **added** | selection is the host's; **free space** `ds_volume_info` (ABI 1.1, Windows) |
| Session: save, load, recent list ("More than 6 days ago") | no | E11.5-S12 |
| Folder Merge (three folders) | no | E11.5-S14.4 and E12-S3 |
| Compare Parent Folders, Locked session | host | UI state |
| Right-click "Select left side / Compare with" from Explorer | **added** | `diffsync_shell.dll` (E11.5-S7), the selection store `ds_shell_selection_*` |
| Source-control clients start the tool | **added** | `ds_tool_parse_args`, the ignore list for lock files, `ds_merge_files` stub |

## WinDiff

Outline of files with a status per file, expand a file to see the line diff, "Only in" lists. All of
it is the folder compare plus `ds_diff_run` (`diff -rq` shaped output and unified output).

## Other session types on the Beyond Compare Home screen

Hex, Media, Picture, Registry, Table and Version compare, and Text Edit, are not part of this product
for now (the owner dropped them from the epic): hex compare stays an E12 item, the others are out of
scope.

## Summary

Everything a UI needs to show text and folder comparisons, navigate, filter, fold, search, report and
launch from Explorer and source-control tools is in the core. **What a UI cannot do yet is change a
text**: copying a section to the other side, editing, undo, save and the three-way merge. That is
E11.5-S14 and is the first thing to build before a UI promises the merge arrows that every reference
product has.
