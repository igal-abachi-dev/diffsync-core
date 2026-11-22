//! `MemVfs`: a deterministic in-memory tree with injectable times, attributes, case rules and
//! file IDs, for unit tests (plan.md §6.2, backlog E5-S11.1). Cloning shares the storage.

use std::collections::{BTreeSet, HashMap};
use std::io::{Cursor, Write};
use std::sync::Arc;

use core_base::{CancelToken, Error, ErrorKind, Result, Ticks};
use parking_lot::Mutex;

use crate::copy::{copy_stream, temp_sibling_name};
use crate::path::{Name, VPath};
use crate::vfs::{CopyOptions, DeleteMode, Entry, Kind, Progress, ReadSeek, Vfs, VfsCaps, attr};

#[derive(Debug, Clone)]
struct Node {
    name: Name,
    kind: Kind,
    data: Vec<u8>,
    mtime: Ticks,
    ctime: Ticks,
    attrs: u32,
    link_target: Option<Kind>,
    file_id: u128,
    children: BTreeSet<String>,
}

#[derive(Debug)]
struct State {
    /// The backend gives no file IDs (a FAT-like volume): overlap checks cannot conclude.
    no_ids: bool,
    nodes: HashMap<String, Node>,
    case_sensitive: bool,
    windows_names: bool,
    seek_penalty: bool,
    next_id: u128,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
}

/// An in-memory `Vfs`.
#[derive(Debug, Clone)]
pub struct MemVfs {
    inner: Arc<Inner>,
}

/// The case-insensitive key of a name: decision: D6 folding per UTF-16 unit (so `ss` and the German
/// sharp s stay different, and distinct lone surrogates stay distinct), written as hex so it is a
/// `String`.
fn fold(units: &[u16]) -> String {
    crate::fold::fold_name(units)
        .iter()
        .map(|u| format!("{u:04x}"))
        .collect()
}

fn key_of(case_sensitive: bool, p: &VPath) -> String {
    p.components()
        .iter()
        .map(|n| {
            if case_sensitive {
                n.display_escaped()
            } else {
                fold(n.units())
            }
        })
        .collect::<Vec<_>>()
        .join("\u{1}")
}

impl State {
    fn key(&self, p: &VPath) -> String {
        key_of(self.case_sensitive, p)
    }

    fn parent_key(&self, p: &VPath) -> Option<String> {
        p.parent().map(|q| self.key(&q))
    }

    fn insert(&mut self, p: &VPath, mut node: Node) -> Result<()> {
        let key = self.key(p);
        let pkey = self
            .parent_key(p)
            .ok_or_else(|| Error::invalid_argument("cannot replace the root"))?;
        match self.nodes.get(&pkey) {
            Some(parent) if parent.kind == Kind::Dir => {}
            Some(_) => {
                return Err(Error::new(ErrorKind::NotFound, "parent is not a folder")
                    .with_path(p.display()));
            }
            None => {
                return Err(
                    Error::new(ErrorKind::NotFound, "parent folder does not exist")
                        .with_path(p.display()),
                );
            }
        }
        node.file_id = self.next_id;
        self.next_id += 1;
        self.nodes
            .get_mut(&pkey)
            .map(|par| par.children.insert(key.clone()));
        self.nodes.insert(key, node);
        Ok(())
    }

    fn remove_subtree(&mut self, key: &str) {
        // explicit stack: no unbounded recursion (AGENTS.md)
        let mut stack = vec![key.to_string()];
        while let Some(k) = stack.pop() {
            if let Some(node) = self.nodes.remove(&k) {
                stack.extend(node.children);
            }
        }
    }

    fn detach(&mut self, p: &VPath) {
        if let (Some(pk), key) = (self.parent_key(p), self.key(p)) {
            if let Some(par) = self.nodes.get_mut(&pk) {
                par.children.remove(&key);
            }
        }
    }
}

impl Default for MemVfs {
    fn default() -> Self {
        Self::new()
    }
}

impl MemVfs {
    /// An empty case-sensitive tree with a root folder.
    pub fn new() -> Self {
        let root = Node {
            name: Name::default(),
            kind: Kind::Dir,
            data: Vec::new(),
            mtime: 0,
            ctime: 0,
            attrs: attr::DIRECTORY,
            link_target: None,
            file_id: 0,
            children: BTreeSet::new(),
        };
        let mut nodes = HashMap::new();
        nodes.insert(String::new(), root);
        MemVfs {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    nodes,
                    case_sensitive: true,
                    windows_names: false,
                    no_ids: false,
                    seek_penalty: false,
                    next_id: 1,
                }),
            }),
        }
    }

    /// Switches name matching to case-insensitive (like NTFS defaults). Call before adding
    /// entries.
    #[must_use]
    pub fn case_insensitive(self) -> Self {
        self.inner.state.lock().case_sensitive = false;
        self
    }

    /// Makes the backend report a rotating disk (`VfsCaps::seek_penalty`), so the read policy of
    /// content compare can be tested.
    #[must_use]
    pub fn rotating_disk(self) -> Self {
        self.inner.state.lock().seek_penalty = true;
        self
    }

    /// Makes the backend report Windows naming rules (invalid characters, device names), so
    /// destination-name validation can be tested without a Windows volume.
    #[must_use]
    pub fn windows_names(self) -> Self {
        self.inner.state.lock().windows_names = true;
        self
    }

    /// Makes the backend give no file IDs, like a FAT or exFAT volume, so the checks that need
    /// them (nested roots, a file copied onto itself) cannot conclude.
    #[must_use]
    pub fn without_file_ids(self) -> Self {
        self.inner.state.lock().no_ids = true;
        self
    }

    fn node(name: &Name, kind: Kind, mtime: Ticks) -> Node {
        Node {
            name: name.clone(),
            kind,
            data: Vec::new(),
            mtime,
            ctime: mtime,
            attrs: if kind == Kind::Dir {
                attr::DIRECTORY
            } else {
                attr::ARCHIVE
            },
            link_target: None,
            file_id: 0,
            children: BTreeSet::new(),
        }
    }

    fn volume(&self) -> u64 {
        Arc::as_ptr(&self.inner) as usize as u64
    }

    /// Adds a folder (and missing parents, with time 0).
    pub fn add_dir(&self, path: &str, mtime: Ticks) -> &Self {
        let p = VPath::from_parts(path.split('/').filter(|s| !s.is_empty()));
        let mut st = self.inner.state.lock();
        let mut cur = VPath::root();
        for (i, c) in p.components().iter().enumerate() {
            cur = cur.join(c.clone());
            let k = st.key(&cur);
            if !st.nodes.contains_key(&k) {
                let t = if i + 1 == p.len() { mtime } else { 0 };
                let _ = st.insert(&cur, Self::node(c, Kind::Dir, t));
            }
        }
        self
    }

    /// Adds a file with content (parents are created).
    pub fn add_file(&self, path: &str, data: &[u8], mtime: Ticks) -> &Self {
        let p = VPath::from_parts(path.split('/').filter(|s| !s.is_empty()));
        if let Some(parent) = p.parent() {
            if !parent.is_root() {
                self.add_dir(&parent.display(), 0);
            }
        }
        let name = p.file_name().cloned().unwrap_or_default();
        let mut node = Self::node(&name, Kind::File, mtime);
        node.data = data.to_vec();
        let mut st = self.inner.state.lock();
        let key = st.key(&p);
        if let Some(n) = st.nodes.get_mut(&key) {
            n.data = data.to_vec();
            n.mtime = mtime;
        } else {
            let _ = st.insert(&p, node);
        }
        self
    }

    /// Adds a link (never followed; reported as a link).
    pub fn add_link(&self, path: &str, target_kind: Kind) -> &Self {
        let p = VPath::from_parts(path.split('/').filter(|s| !s.is_empty()));
        let name = p.file_name().cloned().unwrap_or_default();
        let mut node = Self::node(&name, Kind::Link, 0);
        node.link_target = Some(target_kind);
        let _ = self.inner.state.lock().insert(&p, node);
        self
    }

    /// Sets raw attributes (e.g. `attr::READONLY`) without touching anything else.
    pub fn set_raw_attrs(&self, path: &str, attrs: u32) -> &Self {
        let p = VPath::from_parts(path.split('/').filter(|s| !s.is_empty()));
        let mut st = self.inner.state.lock();
        let k = st.key(&p);
        if let Some(n) = st.nodes.get_mut(&k) {
            n.attrs = attrs
                | if n.kind == Kind::Dir {
                    attr::DIRECTORY
                } else {
                    0
                };
        }
        self
    }

    /// The content of a file, for assertions.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let p = VPath::from_parts(path.split('/').filter(|s| !s.is_empty()));
        let st = self.inner.state.lock();
        st.nodes
            .get(&st.key(&p))
            .filter(|n| n.kind == Kind::File)
            .map(|n| n.data.clone())
    }

    /// Every path in the tree (sorted, `/`-joined), for assertions.
    pub fn paths(&self) -> Vec<String> {
        let st = self.inner.state.lock();
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), String::new())];
        while let Some((key, display)) = stack.pop() {
            if let Some(n) = st.nodes.get(&key) {
                if !key.is_empty() {
                    out.push(display.clone());
                }
                for c in &n.children {
                    if let Some(cn) = st.nodes.get(c) {
                        let d = if display.is_empty() {
                            cn.name.display_escaped()
                        } else {
                            format!("{display}/{}", cn.name.display_escaped())
                        };
                        stack.push((c.clone(), d));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Builds a tree from a JSON recipe:
    /// `{"caseSensitive": false, "entries": [{"path": "a/b.txt", "size": 5, "data": "hello",
    /// "mtime": 123, "attrs": 1, "kind": "file|dir|link"}]}`. `size` fills the file with that
    /// many zero bytes when `data` is absent. (E5-S11.1 asks for `.toml`; JSON is used because
    /// `serde_json` is already a dependency.)
    pub fn from_recipe(json: &str) -> Result<MemVfs> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| Error::invalid_argument(format!("bad recipe: {e}")))?;
        let mut vfs = MemVfs::new();
        if v["caseSensitive"].as_bool() == Some(false) {
            vfs = vfs.case_insensitive();
        }
        for e in v["entries"].as_array().into_iter().flatten() {
            let path = e["path"]
                .as_str()
                .ok_or_else(|| Error::invalid_argument("recipe entry without a path"))?;
            let mtime = e["mtime"].as_i64().unwrap_or(0);
            match e["kind"].as_str().unwrap_or("file") {
                "dir" => {
                    vfs.add_dir(path, mtime);
                }
                "link" => {
                    vfs.add_link(path, Kind::File);
                }
                _ => {
                    let data = e["data"].as_str().map_or_else(
                        || vec![0u8; e["size"].as_u64().unwrap_or(0) as usize],
                        |s| s.as_bytes().to_vec(),
                    );
                    vfs.add_file(path, &data, mtime);
                }
            }
            if let Some(a) = e["attrs"].as_u64() {
                vfs.set_raw_attrs(path, a as u32);
            }
        }
        Ok(vfs)
    }
}

/// Each `MemVfs` instance is its own volume, so two instances never look like one folder.
fn entry_of(volume: u64, n: &Node) -> Entry {
    Entry {
        name: n.name.clone(),
        kind: n.kind,
        size: n.data.len() as u64,
        mtime: n.mtime,
        ctime: n.ctime,
        attrs: n.attrs,
        link_target_kind: n.link_target,
        file_id: Some((volume, n.file_id)),
    }
}

struct MemWriter {
    inner: Arc<Inner>,
    key: String,
}

impl Write for MemWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut st = self.inner.state.lock();
        match st.nodes.get_mut(&self.key) {
            Some(n) => {
                n.data.extend_from_slice(buf);
                Ok(buf.len())
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "temp file was removed",
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn nf(p: &VPath) -> Error {
    Error::new(ErrorKind::NotFound, "no such file or folder").with_path(p.display())
}

impl Vfs for MemVfs {
    fn caps(&self) -> VfsCaps {
        // one guard: the mutex is not re-entrant
        let st = self.inner.state.lock();
        VfsCaps {
            case_sensitive: st.case_sensitive,
            windows_names: st.windows_names,
            seek_penalty: st.seek_penalty,
            time_resolution: 1,
            has_attributes: true,
            ..VfsCaps::default()
        }
    }

    fn stat(&self, p: &VPath) -> Result<Option<Entry>> {
        let st = self.inner.state.lock();
        Ok(st.nodes.get(&st.key(p)).map(|n| entry_of(self.volume(), n)))
    }

    fn list(&self, dir: &VPath, out: &mut Vec<Entry>) -> Result<()> {
        let st = self.inner.state.lock();
        let node = st.nodes.get(&st.key(dir)).ok_or_else(|| nf(dir))?;
        if node.kind != Kind::Dir {
            return Err(Error::invalid_argument("not a folder").with_path(dir.display()));
        }
        out.extend(
            node.children
                .iter()
                .filter_map(|c| st.nodes.get(c))
                .map(|n| entry_of(self.volume(), n)),
        );
        Ok(())
    }

    fn open_read(&self, p: &VPath) -> Result<Box<dyn ReadSeek>> {
        let st = self.inner.state.lock();
        match st.nodes.get(&st.key(p)) {
            Some(n) if n.kind == Kind::File => Ok(Box::new(Cursor::new(n.data.clone()))),
            Some(_) => Err(Error::invalid_argument("not a file").with_path(p.display())),
            None => Err(nf(p)),
        }
    }

    fn create_temp_sibling(&self, p: &VPath) -> Result<(VPath, Box<dyn Write + Send>)> {
        let name = p
            .file_name()
            .ok_or_else(|| Error::invalid_argument("the root has no siblings"))?;
        let parent = p.parent().unwrap_or_default();
        let tmp = parent.join(temp_sibling_name(name));
        let mut node = Self::node(&tmp.file_name().cloned().unwrap_or_default(), Kind::File, 0);
        node.attrs = attr::HIDDEN | attr::ARCHIVE;
        let mut st = self.inner.state.lock();
        st.insert(&tmp, node)?;
        let key = st.key(&tmp);
        drop(st);
        Ok((
            tmp,
            Box::new(MemWriter {
                inner: self.inner.clone(),
                key,
            }),
        ))
    }

    fn copy_file(
        &self,
        src: &VPath,
        dst: &VPath,
        o: &CopyOptions,
        prog: Progress<'_>,
        cancel: &CancelToken,
    ) -> Result<()> {
        copy_stream(self, src, self, dst, o, prog, cancel)
    }

    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        let mut st = self.inner.state.lock();
        let (fk, tk) = (st.key(from), st.key(to));
        if !st.nodes.contains_key(&fk) {
            return Err(nf(from));
        }
        if let Some(dst) = st.nodes.get(&tk) {
            if fk != tk {
                if !replace || dst.kind == Kind::Dir {
                    return Err(Error::new(ErrorKind::AlreadyExists, "destination exists")
                        .with_path(to.display()));
                }
                if dst.attrs & attr::READONLY != 0 {
                    return Err(
                        Error::new(ErrorKind::AccessDenied, "destination is read-only")
                            .with_path(to.display()),
                    );
                }
                st.remove_subtree(&tk);
                st.detach(to);
            }
        }
        if st
            .parent_key(to)
            .is_none_or(|pk| !st.nodes.contains_key(&pk))
        {
            return Err(nf(to));
        }
        // move the node (and a folder's subtree, re-keyed)
        let mut moved = Vec::new();
        let mut stack = vec![(from.clone(), to.clone())];
        while let Some((f, t)) = stack.pop() {
            let key = st.key(&f);
            let Some(mut node) = st.nodes.remove(&key) else {
                continue;
            };
            for c in std::mem::take(&mut node.children) {
                if let Some(child) = st.nodes.get(&c) {
                    let cn = child.name.clone();
                    stack.push((f.join(cn.clone()), t.join(cn)));
                }
            }
            node.name = t.file_name().cloned().unwrap_or_default();
            moved.push((t, node));
        }
        st.detach(from);
        for (t, node) in moved {
            let key = st.key(&t);
            if let Some(pk) = st.parent_key(&t) {
                st.nodes
                    .get_mut(&pk)
                    .map(|p| p.children.insert(key.clone()));
            }
            st.nodes.insert(key, node);
        }
        Ok(())
    }

    fn delete(&self, p: &VPath, mode: DeleteMode) -> Result<()> {
        if mode == DeleteMode::RecycleBin {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "MemVfs has no Recycle Bin",
            ));
        }
        let mut st = self.inner.state.lock();
        let key = st.key(p);
        if p.is_root() {
            return Err(Error::invalid_argument("cannot delete the root"));
        }
        if !st.nodes.contains_key(&key) {
            return Err(nf(p));
        }
        st.detach(p);
        st.remove_subtree(&key); // links are leaves: nothing behind them is touched
        Ok(())
    }

    fn remove_dir_if_empty(&self, p: &VPath) -> Result<bool> {
        if p.is_root() {
            return Err(Error::invalid_argument("cannot delete the root"));
        }
        // one lock for the check and the removal: nothing can be added in between
        let mut st = self.inner.state.lock();
        let key = st.key(p);
        match st.nodes.get(&key) {
            None => return Err(nf(p)),
            Some(n) if n.kind != Kind::Dir => {
                return Err(Error::invalid_argument("not a folder").with_path(p.display()));
            }
            Some(n) if !n.children.is_empty() => return Ok(false),
            Some(_) => {}
        }
        st.detach(p);
        st.remove_subtree(&key);
        Ok(true)
    }

    fn create_dir(&self, p: &VPath) -> Result<()> {
        let mut st = self.inner.state.lock();
        if let Some(n) = st.nodes.get(&st.key(p)) {
            return if n.kind == Kind::Dir {
                Ok(())
            } else {
                Err(Error::new(ErrorKind::AlreadyExists, "a file is in the way")
                    .with_path(p.display()))
            };
        }
        let name = p
            .file_name()
            .ok_or_else(|| Error::invalid_argument("the root exists"))?
            .clone();
        st.insert(p, Self::node(&name, Kind::Dir, 0))
    }

    fn set_times(&self, p: &VPath, mtime: Option<Ticks>, ctime: Option<Ticks>) -> Result<()> {
        let mut st = self.inner.state.lock();
        let k = st.key(p);
        let n = st.nodes.get_mut(&k).ok_or_else(|| nf(p))?;
        if let Some(m) = mtime {
            n.mtime = m;
        }
        if let Some(c) = ctime {
            n.ctime = c;
        }
        Ok(())
    }

    fn set_attrs(&self, p: &VPath, attrs: u32) -> Result<()> {
        let mut st = self.inner.state.lock();
        let k = st.key(p);
        let n = st.nodes.get_mut(&k).ok_or_else(|| nf(p))?;
        n.attrs = attrs
            | if n.kind == Kind::Dir {
                attr::DIRECTORY
            } else {
                0
            };
        Ok(())
    }

    fn file_id(&self, p: &VPath) -> Result<Option<(u64, u128)>> {
        let st = self.inner.state.lock();
        let no_ids = st.no_ids;
        st.nodes
            .get(&st.key(p))
            .map(|n| (!no_ids).then_some((self.volume(), n.file_id)))
            .ok_or_else(|| nf(p))
    }

    fn storage_id(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }
}
