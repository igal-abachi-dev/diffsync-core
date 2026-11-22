//! Fuzz target `options_json` (backlog E11-S1): the JSON options parsers. A bad document is an
//! error, never a panic, and whatever parses must resolve into engine settings without panic.
#![no_main]

use core_diff::TextOptions;
use core_folder::{FolderRules, Status};
use core_sync::SyncRules;
use libfuzzer_sys::fuzz_target;

const STATUSES: [Status; 8] = [
    Status::Same,
    Status::UnimportantDiff,
    Status::Different,
    Status::LeftNewer,
    Status::RightNewer,
    Status::LeftOrphan,
    Status::RightOrphan,
    Status::Error,
];

fuzz_target!(|data: &[u8]| {
    let Ok(json) = std::str::from_utf8(data) else { return };
    if let Ok(o) = TextOptions::from_json(json) {
        let _ = o.line_rules();
        let _ = o.align_config().resolve();
    }
    if let Ok((r, _warnings)) = FolderRules::from_json(json) {
        let _ = r.to_json();
        let _ = r.times_equal(0, i64::MAX / 2);
        let _ = r.times_equal(i64::MIN / 2, i64::MAX / 2);
    }
    if let Ok(r) = SyncRules::from_json(json) {
        for s in STATUSES {
            let _ = r.op_for(s);
        }
    }
});
