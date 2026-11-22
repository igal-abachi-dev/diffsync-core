//! Fuzz target `text_decode` (backlog E11-S1): encoding detection, decoding and line splitting of
//! arbitrary bytes. Nothing may panic, every line must be readable, and the line count must agree
//! with the document.
#![no_main]

use core_text::{DecodeOptions, TextDocument};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = TextDocument::from_bytes(data.to_vec(), &DecodeOptions::default()) else {
        return;
    };
    if doc.is_binary() {
        return;
    }
    let n = doc.line_count();
    let mut total = 0usize;
    for i in 0..n {
        let line = doc.line(i);
        total += line.len();
        let _ = doc.line_ending(i);
        let _ = doc.line_bytes(i);
    }
    assert!(total <= doc.text_bytes().len(), "lines hold more bytes than the text");
    let _ = doc.ending_counts();
    let _ = doc.serialize();
});
