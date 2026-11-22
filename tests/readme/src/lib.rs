//! The Rust examples of the repository README, compiled and run as doc tests so they cannot
//! drift from the API (backlog E11-S5.1). Lines that start with `# ` inside the README code
//! blocks are hidden setup (temp folders and test files).
#![doc = include_str!("../../../README.md")]

// the dependencies are used by the doc tests only
use {
    core_base as _, core_diff as _, core_folder as _, core_sync as _, core_vfs as _,
    parking_lot as _, tempfile as _,
};
