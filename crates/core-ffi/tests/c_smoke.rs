//! Builds `tests/c-smoke/smoke.c` against the generated header and the real DLL, and runs it
//! (backlog E9-S8; Windows only). The C program checks struct sizes and offsets, open/pull/close
//! for text and folder, a stale handle, a too-small buffer, the panic barrier and a sync dry
//! run on a temp tree.
#![cfg(windows)]
#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target_dir() -> PathBuf {
    // the test exe sits somewhere below `target/<profile>`: walk up to that folder
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n == "debug" || n == "release")
        })
        .expect("the test executable is not inside a target/debug or target/release folder")
        .to_path_buf()
}

#[test]
fn the_c_program_passes_against_the_dll() {
    // the cdylib (and its import library) with the test export
    let st = Command::new(env!("CARGO"))
        .args(["build", "-p", "core-ffi", "--features", "test-panic"])
        .current_dir(repo())
        .status()
        .unwrap();
    assert!(st.success(), "cargo build of the DLL failed");
    let target = target_dir();
    let (dll, lib) = (
        target.join("diffsync_core.dll"),
        target.join("diffsync_core.dll.lib"),
    );
    assert!(dll.exists() && lib.exists(), "missing {dll:?} / {lib:?}");

    let work = tempfile::tempdir().unwrap();
    let w = work.path();
    std::fs::write(w.join("left.txt"), "alpha\nbeta1\ngamma\ndelta\n").unwrap();
    std::fs::write(w.join("right.txt"), "alpha\nbeta2\ngamma\ndelta\n").unwrap();
    for (d, files) in [
        ("L", &[("same.txt", "x"), ("only_left.txt", "l")][..]),
        ("R", &[("same.txt", "x"), ("only_right.txt", "r")][..]),
    ] {
        std::fs::create_dir_all(w.join(d)).unwrap();
        for (n, c) in files {
            std::fs::write(w.join(d).join(n), c).unwrap();
        }
    }
    // keep the two same.txt files identical in time as well
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for d in ["L", "R"] {
        std::fs::File::options()
            .write(true)
            .open(w.join(d).join("same.txt"))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    let tool = cc::Build::new()
        .target(env!("TARGET_TRIPLE"))
        .host(env!("TARGET_TRIPLE"))
        .opt_level(0)
        .debug(false)
        .cargo_metadata(false)
        .try_get_compiler()
        .expect("no C compiler found (install the MSVC build tools)");
    let exe = w.join("smoke.exe");
    let mut cmd = tool.to_command();
    cmd.arg("/nologo")
        .arg("/std:c11")
        .arg(repo().join("tests").join("c-smoke").join("smoke.c"))
        .arg(format!("/I{}", repo().join("include").display()))
        .arg(format!("/Fe{}", exe.display()))
        .arg(format!("/Fo{}\\", w.display()))
        .arg(&lib);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "the C program does not compile against include/diffsync.h:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // run it with the DLL next to it
    std::fs::copy(&dll, w.join("diffsync_core.dll")).unwrap();
    let run = Command::new(&exe).arg(w).current_dir(w).output().unwrap();
    assert!(
        run.status.success(),
        "smoke failed ({:?}):\n{}\n{}",
        run.status.code(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("c smoke test passed"));
}
