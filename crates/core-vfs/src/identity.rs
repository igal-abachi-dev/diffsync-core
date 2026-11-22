//! File identity and root overlap (plan.md §6.4, §9.7, backlog E5-S5).

use core_base::Result;

use crate::path::VPath;
use crate::vfs::Vfs;

/// How two roots relate. Sync must refuse anything but `Disjoint` (§9.7); compare may warn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootsRelation {
    /// Both name the same folder (also through `subst`, a mapped drive or a junction).
    Same,
    LeftInsideRight,
    RightInsideLeft,
    Disjoint,
}

/// The identities of `p` and of every ancestor up to the backend root (inclusive). A backend
/// without file IDs gives an empty list.
fn chain(v: &dyn Vfs, p: &VPath) -> Result<Vec<(u64, u128)>> {
    let mut out = Vec::new();
    let mut cur = Some(p.clone());
    while let Some(c) = cur {
        match v.file_id(&c)? {
            Some(id) => out.push(id),
            None => return Ok(Vec::new()),
        }
        cur = c.parent();
    }
    // continue above the backend root, so two backends rooted in one tree can be related
    out.extend(v.ancestor_ids()?);
    Ok(out)
}

/// Resolves both roots to file IDs and walks their parent chains (plan.md §9.7). Because the
/// comparison uses IDs, `subst` drives, mapped drives and junctions that alias one folder are
/// detected. `None` when a backend has no file IDs.
pub fn roots_relation(
    left: &dyn Vfs,
    lp: &VPath,
    right: &dyn Vfs,
    rp: &VPath,
) -> Result<Option<RootsRelation>> {
    let (lc, rc) = (chain(left, lp)?, chain(right, rp)?);
    if lc.is_empty() || rc.is_empty() {
        return Ok(None);
    }
    let (l, r) = (lc[0], rc[0]);
    Ok(Some(if l == r {
        RootsRelation::Same
    } else if lc[1..].contains(&r) {
        // the right root is an ancestor of the left one
        RootsRelation::LeftInsideRight
    } else if rc[1..].contains(&l) {
        RootsRelation::RightInsideLeft
    } else {
        RootsRelation::Disjoint
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemVfs;

    #[test]
    fn nested_same_and_disjoint_on_one_tree() {
        let m = MemVfs::new();
        m.add_dir("a/b/c", 0).add_dir("x", 0);
        let p = |s: &str| VPath::from_parts(s.split('/'));
        let rel = |a: &str, b: &str| roots_relation(&m, &p(a), &m, &p(b)).unwrap();
        assert_eq!(rel("a/b", "a/b"), Some(RootsRelation::Same));
        assert_eq!(rel("a/b/c", "a"), Some(RootsRelation::LeftInsideRight));
        assert_eq!(rel("a", "a/b/c"), Some(RootsRelation::RightInsideLeft));
        assert_eq!(rel("a/b", "x"), Some(RootsRelation::Disjoint));
        // the backend root itself is an ancestor of everything
        assert_eq!(
            roots_relation(&m, &VPath::root(), &m, &p("x")).unwrap(),
            Some(RootsRelation::RightInsideLeft)
        );
    }

    #[test]
    fn two_different_backends_are_disjoint_or_unknown() {
        let (a, b) = (MemVfs::new(), MemVfs::new());
        a.add_dir("d", 0);
        b.add_dir("d", 0);
        let p = VPath::from_parts(["d"]);
        // every MemVfs instance is its own volume; clones share state and so are the same folder
        assert_eq!(
            roots_relation(&a, &p, &b, &p).unwrap(),
            Some(RootsRelation::Disjoint)
        );
        assert_eq!(
            roots_relation(&a, &p, &a.clone(), &p).unwrap(),
            Some(RootsRelation::Same)
        );
    }
}
