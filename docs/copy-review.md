# Copy engine review against robocopy and FastCopy (backlog E11.5-S10)

Question: can the copy engine be trusted for a whole-disk copy, as people use robocopy, FastCopy and
Beyond Compare for? This is a code review of `core-vfs` (`copy.rs`, `local_win32.rs`) and of how
`core-sync` drives it (plan §9 safety rules), against what robocopy and FastCopy do. Apart from the
retry policy added with this review (D57) nothing was measured or load-tested here; the speed
figures are the ones in `docs/v1-definition-of-done.md`.

## Verdict

* **Copying a data disk or a large folder tree to another disk** (files, folders, times, attributes,
  streams, long paths): yes, for what the copy does it is at least as safe as robocopy, and safer in
  several ways (below). Reading is done by the same Win32 call robocopy uses (`CopyFileExW`).
* **Cloning a system disk or a volume whose permissions matter** (NTFS ACLs and owners, symlinks and
  junctions, files you can only read with backup privilege): **not yet equal to `robocopy /COPYALL /B
  /SL /SJ`**. Those are listed below as gaps with the reason each one is deliberate or not done yet.
* **Millions of small files**: correct but single-threaded; robocopy `/MT:n` and FastCopy are faster
  there.

## Where the engine is equal or better

| Concern | robocopy / FastCopy | This engine |
|---|---|---|
| The copy call | `CopyFileEx` (robocopy), own loop (FastCopy) | `CopyFileExW` with a progress routine, so cancel works inside a 500 GB file; streams, attributes, extended attributes and times come with it |
| Interrupted copy | `/Z` restartable, otherwise a half-written destination | **Safe copy** (D13): data goes to `<name>.<tag>.dctmp~` and is renamed over the destination with write-through; a crash or cancel leaves the old file or the new one, plus at most one temp that `ds_maintenance_sweep` removes. Tested by killing a child process mid-copy |
| Source changes during the copy | not detected | detected (D28): the source must have the size and time it had before the copy, else the copy is refused |
| Verification | none (FastCopy can verify) | optional read-back CRC-32 of the copy before the rename (fix B6) |
| Locked file, flaky share | `/R:n /W:s` (default: a million tries, 30 s) | `retries`/`retry_wait_ms` (D57): sharing and lock violations and dropped-share errors, cancel-aware; default 2 tries 1 s in sync jobs, JSON `copyRetries`, `copyRetryWaitMs`. After the tries the job asks Retry/Skip/Skip all/Abort |
| Times | `/COPY:T`, `/DCOPY:T` | file modified time always, creation time optional (`copy_ctime`), folder times restored deepest first after the files |
| Long paths | needs `\\?\` care | every Win32 call uses `\\?\` paths |
| Names | copies what the source has | names the target cannot hold (reserved names, trailing dot, invalid characters, case collisions) are refused in the plan, before anything is written (§9.8) |
| Disk full | error | mapped to `DiskFull`, the temp file is deleted |
| Read-only, hidden, system targets | `/A-:R` tricks | cleared and restored on overwrite (fix B5) |
| Mirror and purge | `/MIR`, `/PURGE` delete without a second look | sync deletes only after re-checking source and destination fingerprints, never deletes files a filter hid, deletes folders item by item and only if empty, deletes a junction as a link and never enters it (§9) |
| Exclusions | `/XF /XD` | glob filters with the same syntax in compare and sync |
| Log | text | JSON lines, one per action, written even when the job aborts |
| Cancel, pause | Ctrl+C | cooperative cancel and pause; nothing half-written afterwards |
| Network shares | `CopyFileEx` (server-side copy possible) | the same call, so the same offload |

## Gaps against robocopy / FastCopy

| Gap | Effect | Why it is open | Where it goes |
|---|---|---|---|
| **NTFS security** (`/SEC`, `/COPYALL`, `/SECFIX`): ACLs, owner, audit entries | A copied file gets the permissions inherited from the destination folder (what Explorer does), not the source's. Wrong for a system or share-permission clone | `CopyFileEx` does not carry the security descriptor; copying it needs `GetNamedSecurityInfo`/`SetNamedSecurityInfo` and, for the owner, a privilege. Not built yet | E12 (copy security as an option, DACL first) |
| **Backup mode** (`/B`, FastCopy's backup privilege) | Files the user has no read access to (other users' profiles, `System Volume Information`) fail with access denied | `CopyFileEx` has no backup-semantics flag; needs `BackupRead`/`BackupWrite` or opening with `FILE_FLAG_BACKUP_SEMANTICS` and a stream copy | E12 |
| **Symbolic links and junctions** (`/SL`, `/SJ`) | Links are skipped and reported (`skipped_links`), never followed and never recreated | Deliberate and safe (D16, D31, §9.5): following a junction while deleting or copying is how data is lost. Recreating them needs privilege detection | E12-S5 |
| **Multithreaded copy** (`/MT:n`) | One file at a time: a disk of millions of small files is slower than `robocopy /MT:16` | The executor is serial so that the §9 ordering (free names, folders, copies, deletes, folder times) and the question handshake stay simple | E12 (a worker pool for the copy phase) |
| **Resume of a huge file after a crash** (`/Z`) | A crash in the middle of a 500 GB file starts that file again from byte 0 | The price of never leaving a half-written destination; resuming needs a checkpoint beside the temp | E13 (FastCDC/delta, §14) |
| **Hard links**, sparse files, compression/dedup state, EFS to a non-NTFS target | Hard links become separate files; sparse files are expanded; an encrypted file cannot go to FAT | The same as robocopy without special switches | not planned |
| **Locked files** (Volume Shadow Copy) | A file open for writing by another program (a database, an Outlook store) cannot be copied | Neither robocopy nor FastCopy does it | not planned |

## Hardening after an external review (decisions D64–D68)

* A folder is removed by a call that refuses a folder with anything in it; a file created a moment
  before is never deleted with it. A rename that must not replace uses a hard link on Unix.
* A copy checks its destination again right before the rename (size and time as at the start of the
  copy) and refuses with `ChangedSinceCompare` otherwise; a small window between that check and the
  rename remains, as with any file system without a compare-and-swap rename.
* A copy is flushed to the storage device before the rename (and the folder entry on Unix), so a
  power loss cannot leave a truncated file under the good name. `durableCopy` can be turned off for
  speed.
* A cross-volume move deletes only items its own copy produced; a link stays where it is.
* **What the review measured and is still true:** a copy between two different roots streams the data
  (`CopyFileExW` is used only inside one volume), so NTFS alternate data streams and ACLs are not
  carried there either; the 0.71 × robocopy figure is the native path. Planned with S10.3.
* A Mirror now copies differences the compare calls unimportant (D67).

## What this means for the product

* The UI should tell the user which of the gaps their copy touches: "links skipped (n)" is already in
  the plan summary; "permissions not copied" is a one-line note in the confirmation until the
  security option exists.
* For a system-disk clone, recommend an imaging tool or `robocopy /COPYALL /B /SL /SJ /MT` until E12
  closes the security, backup-mode, link and thread gaps.
