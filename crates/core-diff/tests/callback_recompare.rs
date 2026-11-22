//! A host that recompares from inside its event callback (an external review's harness; the
//! README's C# example posts synchronously, `post: action => action()`). The callback runs on the
//! worker thread, so `recompare` is called from the very thread it has to stop: it must neither
//! deadlock nor leave rows of the old run in the new run's store.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use core_diff::options::AlignmentName;
use core_diff::session::event;
use core_diff::{DiffRow, DocSpec, RowKind, Side, Status, TextCompare, TextOptions};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn lines(rng: &mut Rng, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| format!("line {}", rng.next() % 50))
        .collect()
}

#[test]
fn recompare_from_the_event_callback_keeps_rows_consistent() {
    for round in 0..30u64 {
        let mut rng = Rng(0x1234_5678 + round * 31);
        let a = lines(&mut rng, 6000);
        let mut b = a.clone();
        for k in 0..20 {
            let at = (rng.next() as usize) % b.len();
            if k % 2 == 0 {
                b.remove(at);
            } else {
                b.insert(at, format!("inserted {k}"));
            }
        }
        let (ba, bb) = (a.join("\n").into_bytes(), b.join("\n").into_bytes());
        let slot: Arc<OnceLock<Arc<TextCompare>>> = Arc::new(OnceLock::new());
        let fired = Arc::new(AtomicBool::new(false));
        let (s2, f2) = (slot.clone(), fired.clone());
        let sink: core_diff::session::EventSink = Arc::new(move |bits| {
            if bits & event::ROWS != 0 && !f2.swap(true, Ordering::SeqCst) {
                if let Some(tc) = s2.get() {
                    let o = TextOptions {
                        ignore_case: true,
                        ..TextOptions::default()
                    };
                    tc.recompare(o).unwrap();
                }
            }
        });
        let o = TextOptions {
            alignment: AlignmentName::None, // fast, many batches
            ..TextOptions::default()
        };
        let tc = TextCompare::start(DocSpec::Bytes(ba), DocSpec::Bytes(bb), o, Some(sink)).unwrap();
        let _ = slot.set(tc.clone());
        let start = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if fired.load(Ordering::SeqCst) && tc.status() == Status::Done {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if tc.status() == Status::Done {
                    break;
                }
            }
            assert!(
                start.elapsed().as_secs() < 30,
                "round {round}: the compare never settled (a deadlock in recompare from a callback?)"
            );
        }
        tc.wait();
        let n = tc.row_count();
        let mut rows = vec![
            DiffRow {
                left: 0,
                right: 0,
                kind: RowKind::Same
            };
            n as usize
        ];
        tc.rows(0, &mut rows);
        let (mut nl, mut nr) = (0u32, 0u32);
        for row in &rows {
            if let Some(l) = row.left_opt() {
                assert_eq!(l, nl, "round {round}: left lines out of order");
                nl += 1;
            }
            if let Some(r) = row.right_opt() {
                assert_eq!(r, nr, "round {round}: right lines out of order");
                nr += 1;
            }
        }
        assert_eq!(
            nl,
            tc.doc_info(Side::Left).unwrap().line_count,
            "round {round}"
        );
        assert_eq!(
            nr,
            tc.doc_info(Side::Right).unwrap().line_count,
            "round {round}"
        );
    }
}
