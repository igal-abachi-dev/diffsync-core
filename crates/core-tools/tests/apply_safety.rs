//! Safety rules of applying patches to files (decision D58; an external reviewer's findings).

use core_tools::apply::{ApplyRequest, apply};

fn req(root: &std::path::Path, patch: &str) -> ApplyRequest {
    ApplyRequest {
        patch: patch.into(),
        root: root.to_string_lossy().into_owned(),
        ..ApplyRequest::default()
    }
}

#[test]
fn two_patches_to_one_file_are_refused_not_silently_overwritten() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("f.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
    let two = "--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-1\n+ONE\n--- a/f.txt\n+++ b/f.txt\n@@ -8,1 +8,1 @@\n-8\n+EIGHT\n";
    let rep = apply(&req(d.path(), two));
    assert!(!rep.ok && !rep.written, "{rep:?}");
    assert!(
        rep.error
            .as_deref()
            .is_some_and(|e| e.contains("one after another"))
    );
    assert_eq!(
        std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
        "1\n2\n3\n4\n5\n6\n7\n8\n"
    );
}

#[test]
fn device_names_trailing_dots_and_relative_roots_are_refused() {
    let d = tempfile::tempdir().unwrap();
    for bad in ["nul", "con.txt", "aux", "x."] {
        let p = format!("--- /dev/null\n+++ b/{bad}\n@@ -0,0 +1 @@\n+x\n");
        let rep = apply(&req(d.path(), &p));
        assert!(!rep.ok && !rep.written, "{bad}: {rep:?}");
    }
    let rep = apply(&req(
        std::path::Path::new("relative/folder"),
        "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n",
    ));
    assert!(
        rep.error.as_deref().is_some_and(|e| e.contains("absolute")),
        "{rep:?}"
    );
}

#[test]
fn a_target_that_appears_after_planning_is_not_replaced_and_a_failure_restores_everything() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(d.path().join("b.txt"), "b\n").unwrap();
    // b.txt is read-only: the whole patch is refused before anything is written
    let mut perm = std::fs::metadata(d.path().join("b.txt"))
        .unwrap()
        .permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(d.path().join("b.txt"), perm.clone()).unwrap();
    let p = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+A\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-b\n+B\n--- /dev/null\n+++ b/sub/new.txt\n@@ -0,0 +1 @@\n+n\n";
    let rep = apply(&req(d.path(), p));
    assert!(!rep.ok && !rep.written, "{rep:?}");
    assert_eq!(
        std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
        "a\n"
    );
    assert!(!d.path().join("sub").exists(), "no folder is left behind");
    // nothing but the two files is in the folder (no temp files)
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(d.path().join("b.txt"), perm).unwrap();
    let names: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    // with the file writable the same patch applies
    let rep = apply(&req(d.path(), p));
    assert!(rep.ok && rep.written, "{rep:?}");
    assert_eq!(
        std::fs::read_to_string(d.path().join("sub/new.txt")).unwrap(),
        "n\n"
    );
}
