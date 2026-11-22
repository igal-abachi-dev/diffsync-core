//! Backlog E5-S4.2, decision D52: `LocalStd` measures case sensitivity instead of assuming it from
//! the OS. These tests assert what a CI temp folder is on each platform (Windows NTFS and macOS
//! APFS as installed are case insensitive; a Linux temp folder is case sensitive).

use core_vfs::{LocalStd, VPath, Vfs};

const EXPECTED: bool = !cfg!(any(windows, target_os = "macos"));

fn touch(root: &std::path::Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

#[test]
fn a_folder_with_letters_in_its_names_is_measured() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "Alpha.txt");
    let v = LocalStd::new(d.path());
    assert_eq!(v.dir_case_sensitive(&VPath::root()).unwrap(), EXPECTED);
    assert_eq!(v.caps().case_sensitive, EXPECTED);
}

#[test]
fn folders_without_letters_use_the_file_system_around_them() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "Report.txt");
    std::fs::create_dir_all(d.path().join("123").join("456")).unwrap(); // nothing to swap in 123
    std::fs::write(d.path().join("123").join("789"), b"x").unwrap();
    let v = LocalStd::new(d.path());
    let nested = VPath::root().join(core_vfs::Name::new("123"));
    assert_eq!(v.dir_case_sensitive(&nested).unwrap(), EXPECTED);
}

#[test]
fn an_empty_root_gets_the_platform_default() {
    let d = tempfile::tempdir().unwrap();
    let v = LocalStd::new(d.path());
    assert_eq!(v.caps().case_sensitive, EXPECTED);
}

/// Both spellings listed as separate entries prove a case sensitive folder, wherever they exist
/// (impossible to create on a case insensitive file system, so only there the test runs).
#[test]
fn two_spellings_as_separate_entries_mean_sensitive() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("Mixed.txt"), b"1").unwrap();
    if std::fs::write(d.path().join("MIXED.TXT"), b"2").is_err()
        || std::fs::read_dir(d.path()).unwrap().count() != 2
    {
        return; // this temp folder folds case: nothing to prove here
    }
    let v = LocalStd::new(d.path());
    assert!(v.dir_case_sensitive(&VPath::root()).unwrap());
}
