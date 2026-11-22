//! Fixed-layout ABI types (plan.md §10.3). Every struct is `repr(C)` and its size is asserted
//! here and again in `tests/c-smoke`. Fields are never reordered; new fields go at the end.
//! Enum numbering is never changed.

use std::ffi::c_void;

pub type DsHandle = u64;

/// A UTF-16 string view (paths, file names, line text). Not NUL-terminated.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DsStr16 {
    pub ptr: *const u16,
    pub len: usize,
}

/// A UTF-8 string view (JSON). Not NUL-terminated.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DsStr8 {
    pub ptr: *const u8,
    pub len: usize,
}

/// A list of UTF-16 strings.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DsStr16List {
    pub items: *const DsStr16,
    pub count: usize,
}

/// A buffer allocated by the core; free it with `ds_buf_free`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DsBuf {
    pub ptr: *mut u8,
    pub len: usize,
}

/// Event callback: `(user, source handle, kind, a, b)`. Called on core worker threads only.
pub type DsEventFn = Option<unsafe extern "C" fn(*mut c_void, DsHandle, u32, u64, u64)>;

/// Log sink: `(user, level, UTF-8 message ptr, len)`. Level: 0 error, 1 warning, 2 info.
pub type DsLogFn = Option<unsafe extern "C" fn(*mut c_void, u32, *const u8, usize)>;

pub const DS_EV_STATE: u32 = 1;
pub const DS_EV_ROWS: u32 = 2;
pub const DS_EV_PROGRESS: u32 = 3;
pub const DS_EV_NODE: u32 = 4;
pub const DS_EV_QUESTION: u32 = 5;
pub const DS_EV_LOG: u32 = 6;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DsInitOptions {
    /// `size_of::<DsInitOptions>()` of the caller's header.
    pub struct_size: u32,
    /// Worker threads of the shared pool; 0 = number of cores.
    pub worker_threads: u32,
    pub log_fn: DsLogFn,
    pub log_user: *mut c_void,
    /// Default folder for execution logs (UTF-16, may be empty).
    pub log_dir: DsStr16,
    /// Folder for temporary data (UTF-16, may be empty; reserved).
    pub temp_dir: DsStr16,
}

/// One display row of a text compare (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DsTextRow {
    /// `u32::MAX` = absent on that side.
    pub left_line: u32,
    pub right_line: u32,
    /// `RowKind` value.
    pub kind: u8,
    /// bit 0: first row of a section; bit 1: important difference.
    pub flags: u8,
    pub reserved: u16,
    /// Index of the section the row belongs to, `u32::MAX` for equal rows.
    pub section: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsSection {
    pub first_row: u32,
    pub row_count: u32,
    pub important: u8,
    pub pad: [u8; 3],
}

/// A highlighted range inside a line, in UTF-16 units (12 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsRange {
    pub start: u32,
    pub len: u32,
    /// 1 = important difference, 0 = unimportant.
    pub kind: u8,
    pub pad: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsLineInfo {
    /// 0 none (last line), 1 CRLF, 2 LF, 3 CR, 255 unknown.
    pub ending: u8,
    pub pad: [u8; 3],
    /// Length of the line in UTF-16 units.
    pub utf16_len: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsTextStatus {
    /// 0 loading, 1 aligning, 2 done, 3 failed (message in `ds_last_error`), 4 cancelled,
    /// 5 binary.
    pub state: u32,
    pub rows: u32,
    pub generation: u64,
    pub same: u32,
    pub important_diff: u32,
    pub unimportant_diff: u32,
    pub left_orphan: u32,
    pub right_orphan: u32,
    pub unimportant_left_orphan: u32,
    pub unimportant_right_orphan: u32,
    pub sections: u32,
    pub important_sections: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsDocInfo {
    pub line_count: u32,
    pub is_binary: u8,
    pub has_bom: u8,
    pub pad: [u8; 2],
    pub byte_len: u64,
    /// Lines per ending: none, CRLF, LF, CR.
    pub ending_counts: [u64; 4],
    /// Encoding name, UTF-8, NUL padded.
    pub encoding: [u8; 24],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsFolderStatus {
    /// 0 scanning, 1 done, 2 failed (message in `ds_last_error`), 3 cancelled.
    pub state: u32,
    /// `Stage` value of the current work (4 scanning, 5 comparing content, 10 done).
    pub stage: u32,
    pub progress_done: u64,
    pub progress_total: u64,
    pub node_count: u32,
    pub warning_count: u32,
    pub sync_blocked: u8,
    pub pad: [u8; 3],
    pub diff: u32,
    pub left_newer: u32,
    pub right_newer: u32,
    pub left_orphan: u32,
    pub right_orphan: u32,
    pub error: u32,
    pub items: u32,
    /// Increases whenever the tree changes (refresh, content compare, sync).
    pub revision: u64,
}

/// What one side knows about a node (32 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsSideInfo {
    pub size: u64,
    pub mtime: i64,
    /// Not tracked per node in v1: always 0.
    pub ctime: i64,
    pub attrs: u32,
    pub present: u8,
    /// 0 file, 1 folder, 2 link.
    pub kind: u8,
    pub reserved: u16,
}

/// One row of the folder view, and the info of a node (88 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsDirRow {
    pub node: u32,
    pub parent: u32,
    pub depth: u16,
    /// `Status` value: 0 same, 1 unimportant diff, 2 different, 3 left newer, 4 right newer,
    /// 5 left orphan, 6 right orphan, 7 error.
    pub status: u8,
    /// Node flags: 1 pending content, 2 content compared, 4 type conflict, 8 duplicate key,
    /// 16 offline, 32 link, 64 filtered children, 128 gone.
    pub flags: u8,
    pub child_count: u32,
    pub left: DsSideInfo,
    pub right: DsSideInfo,
    pub diff_count: u32,
    pub orphan_count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsPlanSummary {
    pub copies_to_left: u32,
    pub copies_to_right: u32,
    pub folders_created: u32,
    pub deletes_left: u32,
    pub deletes_right: u32,
    pub conflicts: u32,
    pub errors: u32,
    pub skipped_links: u32,
    pub bytes_to_left: u64,
    pub bytes_to_right: u64,
    pub action_count: u32,
    pub filters_active: u8,
    pub pad: [u8; 3],
}

/// One planned action (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsPlanAction {
    /// 0 create folder left, 1 create folder right, 2 copy to left, 3 copy to right,
    /// 4 delete left, 5 delete right.
    pub kind: u8,
    pub is_folder: u8,
    /// bit 0: delete the folder only if empty; bit 1: frees a name for a type-conflict copy.
    pub flags: u16,
    pub node: u32,
    pub bytes: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsJobStatus {
    /// 0 running, 1 paused, 2 finished.
    pub state: u32,
    pub steps_total: u32,
    pub steps_done: u32,
    pub has_question: u8,
    pub pad: [u8; 3],
    pub bytes_total: u64,
    pub bytes_done: u64,
    /// Valid when `state == 2`.
    pub done: u32,
    pub skipped: u32,
    pub failed: u32,
    pub kept: u32,
    pub bytes_copied: u64,
    pub aborted: u8,
    pub cancelled: u8,
    pub pad2: [u8; 6],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsQuestion {
    /// 0 = no question is open.
    pub id: u64,
    /// 0 changed since compare, 1 failed.
    pub kind: u32,
    pub step: u32,
}

/// A run of rows the Context view hides (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsFold {
    pub first_row: u32,
    pub row_count: u32,
}

/// A match of `ds_text_find` (20 bytes): the row, the line on `side` (0 left, 1 right), and the
/// range in UTF-16 units.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsFindHit {
    pub row: u32,
    pub line: u32,
    pub start: u32,
    pub end: u32,
    pub side: u8,
    pub pad: [u8; 3],
}

/// Free and total bytes of a volume (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DsVolumeInfo {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

const _: () = {
    assert!(size_of::<DsFold>() == 8);
    assert!(size_of::<DsFindHit>() == 20);
    assert!(size_of::<DsVolumeInfo>() == 16);
    assert!(size_of::<DsTextRow>() == 16);
    assert!(size_of::<DsRange>() == 12);
    assert!(size_of::<DsSection>() == 12);
    assert!(size_of::<DsLineInfo>() == 8);
    assert!(size_of::<DsSideInfo>() == 32);
    assert!(size_of::<DsDirRow>() == 88);
    assert!(size_of::<DsPlanAction>() == 16);
    assert!(size_of::<DsStr16>() == 2 * size_of::<usize>());
    assert!(size_of::<DsBuf>() == 2 * size_of::<usize>());
    assert!(size_of::<DsQuestion>() == 16);
    assert!(size_of::<DsTextStatus>() == 56);
    assert!(size_of::<DsDocInfo>() == 72);
    assert!(size_of::<DsFolderStatus>() == 72);
    assert!(size_of::<DsPlanSummary>() == 56);
    assert!(size_of::<DsJobStatus>() == 64);
    assert!(std::mem::offset_of!(DsTextStatus, generation) == 8);
    assert!(std::mem::offset_of!(DsFolderStatus, revision) == 64);
    assert!(std::mem::offset_of!(DsJobStatus, bytes_copied) == 48);
};

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<DsInitOptions>() == 56);
    assert!(std::mem::offset_of!(DsInitOptions, log_dir) == 24);
};
