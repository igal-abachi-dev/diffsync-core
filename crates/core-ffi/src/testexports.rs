//! Exports that exist only with the `test-panic` feature. Not part of the generated header or
//! the C# bindings (`cbindgen:ignore`; the generators read the other modules only).
#![allow(unsafe_code)]

use crate::status::{DS_OK, DsStatus, ffi_entry};

/// Panics inside the barrier and must return `DS_E_INTERNAL`.
///
/// cbindgen:ignore
#[unsafe(no_mangle)]
pub extern "C" fn ds_test_panic() -> DsStatus {
    ffi_entry!({
        let v: Vec<u8> = Vec::new();
        let _ = v[std::hint::black_box(3)];
        Ok(())
    })
}

#[allow(dead_code)]
const _: DsStatus = DS_OK;
