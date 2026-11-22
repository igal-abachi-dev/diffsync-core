//! Decision D57 (backlog E11.5-S10.2): a copy that fails with a transient error is tried again, like
//! robocopy's `/R` and `/W`; nothing else is retried, and a cancel ends the wait.

use std::time::{Duration, Instant};

use core_base::{CancelToken, ErrorKind};
use core_vfs::copy::{copy_between, is_transient};
use core_vfs::{CopyOptions, FaultVfs, MemVfs, Name, Op, VPath, Vfs};

const T0: i64 = 133_000_000_000_000_000;

fn src_with_one_failure(kind: ErrorKind) -> (FaultVfs<MemVfs>, MemVfs) {
    let l = MemVfs::new();
    l.add_file("a.txt", b"payload", T0);
    let l = FaultVfs::new(l);
    l.fail_nth(Op::OpenRead, 1, kind);
    (l, MemVfs::new())
}

fn copy(
    l: &FaultVfs<MemVfs>,
    r: &MemVfs,
    o: &CopyOptions,
    cancel: &CancelToken,
) -> core_base::Result<()> {
    copy_between(
        l,
        &VPath::root().join(Name::new("a.txt")),
        r,
        &VPath::root().join(Name::new("a.txt")),
        o,
        &|_, _| {},
        cancel,
    )
}

fn fast(retries: u32) -> CopyOptions {
    CopyOptions {
        retries,
        retry_wait_ms: 10,
        ..CopyOptions::default()
    }
}

#[test]
fn a_sharing_violation_is_retried_and_then_succeeds() {
    let (l, r) = src_with_one_failure(ErrorKind::SharingViolation);
    copy(&l, &r, &fast(1), &CancelToken::new()).unwrap();
    assert_eq!(l.count(Op::OpenRead), 2, "one failure, one retry");
    assert!(
        r.stat(&VPath::root().join(Name::new("a.txt")))
            .unwrap()
            .is_some()
    );
}

#[test]
fn without_retries_or_with_too_few_the_error_stands() {
    let (l, r) = src_with_one_failure(ErrorKind::SharingViolation);
    let e = copy(&l, &r, &fast(0), &CancelToken::new()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::SharingViolation);
    assert_eq!(l.count(Op::OpenRead), 1);
    // two failures and one retry: still an error
    let l2 = MemVfs::new();
    l2.add_file("a.txt", b"x", T0);
    let l2 = FaultVfs::new(l2);
    l2.fail_nth(Op::OpenRead, 1, ErrorKind::SharingViolation)
        .fail_nth(Op::OpenRead, 2, ErrorKind::SharingViolation);
    assert!(copy(&l2, &MemVfs::new(), &fast(1), &CancelToken::new()).is_err());
}

#[test]
fn only_transient_errors_are_retried() {
    for kind in [
        ErrorKind::AccessDenied,
        ErrorKind::DiskFull,
        ErrorKind::NotFound,
        ErrorKind::Io,
    ] {
        let (l, r) = src_with_one_failure(kind);
        let e = copy(&l, &r, &fast(3), &CancelToken::new()).unwrap_err();
        assert_eq!(e.kind(), kind);
        assert_eq!(l.count(Op::OpenRead), 1, "{kind:?} must not be retried");
    }
    // a dropped network share is transient whatever kind it was mapped to
    let net = core_base::Error::from_win32(64, "the network name is no longer available");
    assert!(is_transient(&net));
    assert!(is_transient(&core_base::Error::from_win32(
        121,
        "semaphore timeout"
    )));
    assert!(!is_transient(&core_base::Error::from_win32(
        5,
        "access denied"
    )));
}

#[test]
fn a_cancel_ends_the_wait_between_tries() {
    let (l, r) = src_with_one_failure(ErrorKind::SharingViolation);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        c2.cancel();
    });
    let o = CopyOptions {
        retries: 5,
        retry_wait_ms: 60_000,
        ..CopyOptions::default()
    };
    let t = Instant::now();
    let e = copy(&l, &r, &o, &cancel).unwrap_err();
    stopper.join().unwrap();
    assert!(e.is_cancelled(), "{e:?}");
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "the wait is not interruptible"
    );
}
