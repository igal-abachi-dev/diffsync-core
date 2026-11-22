//! The final step of a copy and a rename (decisions D64, D65): the destination is checked again
//! right before it is replaced, a rename that must not replace never does, and an empty-folder
//! removal never takes anything with it.

use core_base::{CancelToken, ErrorKind};
use core_vfs::{CopyOptions, FaultVfs, MemVfs, Op, VPath, Vfs, copy_between};

const T0: i64 = 1_000_000 * 10_000_000;

fn p(s: &str) -> VPath {
    VPath::from_parts(s.split('/').filter(|x| !x.is_empty()))
}

#[test]
fn a_destination_that_changed_during_the_copy_is_not_replaced() {
    let (src, dst) = (MemVfs::new(), MemVfs::new());
    src.add_file("a.txt", b"new content", T0 + 100);
    dst.add_file("a.txt", b"old", T0);
    let dc = dst.clone();
    let fdst = FaultVfs::new(dst);
    // the temp file is created after the copy looked at the destination: change it right there
    fdst.on_nth(
        Op::CreateTemp,
        1,
        std::sync::Arc::new(move |_| {
            dc.add_file("a.txt", b"someone else wrote this meanwhile", T0 + 200);
        }),
    );
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let e = copy_between(
        &src,
        &p("a.txt"),
        &fdst,
        &p("a.txt"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare, "{e}");
    assert_eq!(
        fdst.inner().read("a.txt").unwrap(),
        b"someone else wrote this meanwhile"
    );
    assert_eq!(fdst.inner().paths().len(), 1, "no temp file is left");
}

#[test]
fn a_file_that_appeared_during_a_copy_to_a_new_name_is_not_replaced() {
    let (src, dst) = (MemVfs::new(), MemVfs::new());
    src.add_file("n.txt", b"mine", T0);
    let dc = dst.clone();
    let fdst = FaultVfs::new(dst);
    fdst.on_nth(
        Op::CreateTemp,
        1,
        std::sync::Arc::new(move |_| {
            dc.add_file("n.txt", b"theirs", T0);
        }),
    );
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let r = copy_between(
        &src,
        &p("n.txt"),
        &fdst,
        &p("n.txt"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    );
    assert!(r.is_err());
    assert_eq!(fdst.inner().read("n.txt").unwrap(), b"theirs");
}

#[test]
fn removing_a_folder_that_is_not_empty_removes_nothing() {
    let m = MemVfs::new();
    m.add_file("d/x.txt", b"1", T0).add_dir("e", T0);
    assert!(!m.remove_dir_if_empty(&p("d")).unwrap());
    assert_eq!(m.read("d/x.txt").unwrap(), b"1");
    assert!(m.remove_dir_if_empty(&p("e")).unwrap());
    assert!(m.remove_dir_if_empty(&p("e")).is_err(), "gone");
    assert!(m.remove_dir_if_empty(&VPath::root()).is_err());
}

#[test]
fn the_real_disk_backends_remove_only_empty_folders_and_never_replace_on_rename() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    std::fs::create_dir_all(root.join("full")).unwrap();
    std::fs::write(root.join("full/keep.txt"), "x").unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    let v: Box<dyn Vfs> = {
        #[cfg(windows)]
        {
            Box::new(core_vfs::LocalWin32::new(
                core_vfs::VfsRoot::parse_windows(root.to_str().unwrap()).unwrap(),
            ))
        }
        #[cfg(not(windows))]
        {
            Box::new(core_vfs::LocalStd::new(&root))
        }
    };
    assert!(!v.remove_dir_if_empty(&p("full")).unwrap());
    assert!(root.join("full/keep.txt").exists());
    assert!(v.remove_dir_if_empty(&p("empty")).unwrap());
    assert!(!root.join("empty").exists());
    // a rename that must not replace leaves both files alone
    let e = v.rename(&p("a.txt"), &p("b.txt"), false).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a");
    v.rename(&p("a.txt"), &p("c.txt"), false).unwrap();
    assert!(root.join("c.txt").exists() && !root.join("a.txt").exists());
    // data is flushed: the calls work on a real file
    v.sync_file(&p("c.txt")).unwrap();
    v.sync_parent(&p("c.txt")).unwrap();
}

/// D65: the flush before the rename must work on a read-only source's copy (the temp file carries
/// the read-only attribute, and a handle with write access cannot be opened on it).
#[test]
fn a_read_only_file_is_copied_with_the_durable_flush_on_a_real_disk() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    std::fs::write(root.join("src.txt"), "data").unwrap();
    let mut perm = std::fs::metadata(root.join("src.txt"))
        .unwrap()
        .permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(root.join("src.txt"), perm.clone()).unwrap();
    let backends: Vec<Box<dyn Vfs>> = vec![
        #[cfg(windows)]
        Box::new(core_vfs::LocalWin32::new(
            core_vfs::VfsRoot::parse_windows(root.to_str().unwrap()).unwrap(),
        )),
        Box::new(core_vfs::LocalStd::new(&root)),
    ];
    for (i, v) in backends.iter().enumerate() {
        let dst = format!("copy{i}.txt");
        let o = CopyOptions {
            durable: true,
            ..CopyOptions::default()
        };
        copy_between(
            &**v,
            &p("src.txt"),
            &**v,
            &p(&dst),
            &o,
            &|_, _| {},
            &CancelToken::new(),
        )
        .unwrap_or_else(|e| panic!("backend {i}: {e}"));
        assert_eq!(std::fs::read_to_string(root.join(&dst)).unwrap(), "data");
    }
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    let _ = std::fs::set_permissions(root.join("src.txt"), perm);
}
