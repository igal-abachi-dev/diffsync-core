//! Placeholder target (backlog E0-S6.4). Real targets arrive with E1 (decoder and line
//! splitter), E2-S9.5 (`align_small_skew`), E9 (options JSON, FFI entry points).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Keeps `core-base` linked so the fuzz build exercises the workspace crates.
    let _ = core_base::crc32::line_hash_utf16le(data.iter().map(|&b| u16::from(b)));
});
