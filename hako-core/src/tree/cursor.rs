use super::node::load_node;
use super::types::*;
use crate::hash::Hash;
use crate::store::ChunkStore;
use std::io;

/// Stack-based forward cursor over a prolly tree's leaf entries with
/// **deferred descent**: after bubbling to the next sibling, the cursor
/// stops at the internal frame pointing at the next child without loading
/// that child. Callers can `peek_next_subtree()` to see the next child's
/// hash and `skip_next_subtree()` to advance past it without ever paying
/// the load. Calling `current()` or `next()` materializes the leaf.
pub struct Cursor<'a> {
    store: &'a dyn ChunkStore,
    stack: Vec<Frame>,
    done: bool,
}

struct Frame {
    node: Node,
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Open a cursor at the start of the tree. Lazily descended: the root
    /// is loaded but no descent happens until `current()`/`next()` is called.
    pub fn open(store: &'a dyn ChunkStore, root: Hash) -> io::Result<Self> {
        let mut c = Cursor {
            store,
            stack: Vec::new(),
            done: true,
        };
        if root == Hash::zero() {
            return Ok(c);
        }
        let node = load_node(store, &root)?;
        c.stack.push(Frame { node, pos: 0 });
        c.done = false;
        Ok(c)
    }

    /// Position at the first entry with key >= `key`. Loads only the path
    /// from root to the target leaf.
    pub fn seek(&mut self, key: &[u8]) -> io::Result<()> {
        if self.stack.is_empty() {
            self.done = true;
            return Ok(());
        }
        let root_frame = self.stack.swap_remove(0);
        self.stack.clear();
        self.stack.push(root_frame);
        self.done = false;

        loop {
            enum Step {
                Done,
                PastEnd,
                Descend(Hash),
                LeafExhausted,
            }
            let step = {
                let frame = self.stack.last_mut().unwrap();
                match &frame.node.kind {
                    NodeKind::Leaf { entries } => {
                        let idx = entries
                            .binary_search_by(|e| e.key.as_slice().cmp(key))
                            .unwrap_or_else(|i| i);
                        if idx >= entries.len() {
                            frame.pos = entries.len();
                            Step::LeafExhausted
                        } else {
                            frame.pos = idx;
                            Step::Done
                        }
                    }
                    NodeKind::Internal {
                        child_keys,
                        child_hashes,
                        ..
                    } => {
                        let mut idx = child_keys.len();
                        for (i, k) in child_keys.iter().enumerate() {
                            if k.as_slice() >= key {
                                idx = i;
                                break;
                            }
                        }
                        if idx >= child_keys.len() {
                            Step::PastEnd
                        } else {
                            frame.pos = idx;
                            Step::Descend(child_hashes[idx])
                        }
                    }
                }
            };
            match step {
                Step::Done => return Ok(()),
                Step::PastEnd => {
                    self.done = true;
                    return Ok(());
                }
                Step::LeafExhausted => {
                    self.stack.pop();
                    self.bump_parent_then_descend_leftmost()?;
                    return Ok(());
                }
                Step::Descend(h) => {
                    let child = load_node(self.store, &h)?;
                    self.stack.push(Frame {
                        node: child,
                        pos: 0,
                    });
                }
            }
        }
    }

    /// Borrow the entry under the cursor. Materializes any pending descent.
    pub fn current(&mut self) -> io::Result<Option<(&[u8], &Value)>> {
        self.realize()?;
        if self.done {
            return Ok(None);
        }
        let frame = self.stack.last().unwrap();
        match &frame.node.kind {
            NodeKind::Leaf { entries } => {
                let e = &entries[frame.pos];
                Ok(Some((&e.key, &e.value)))
            }
            _ => unreachable!("realize ensures leaf or done"),
        }
    }

    /// Yield the entry under the cursor and advance.
    // Deliberately named `next`, but this is a fallible streaming cursor
    // (returns `io::Result`), not an `Iterator` — the standard trait can't
    // express the I/O. The name mirrors the cursor vocabulary on purpose.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<Option<(Vec<u8>, Value)>> {
        self.realize()?;
        if self.done {
            return Ok(None);
        }
        let entry = {
            let frame = self.stack.last().unwrap();
            match &frame.node.kind {
                NodeKind::Leaf { entries } => {
                    let e = &entries[frame.pos];
                    (e.key.clone(), e.value.clone())
                }
                _ => unreachable!(),
            }
        };
        let exhausted = {
            let frame = self.stack.last_mut().unwrap();
            frame.pos += 1;
            match &frame.node.kind {
                NodeKind::Leaf { entries } => frame.pos >= entries.len(),
                _ => unreachable!(),
            }
        };
        if exhausted {
            self.stack.pop();
            self.bump_parent()?;
        }
        Ok(Some(entry))
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// Hash of the next subtree the cursor would descend into, without
    /// loading. Returns `None` if the cursor is already at a leaf entry
    /// (no pending descent), is done, or has exhausted all internal frames.
    pub fn peek_next_subtree(&self) -> Option<Hash> {
        if self.done {
            return None;
        }
        let frame = self.stack.last()?;
        match &frame.node.kind {
            NodeKind::Internal { child_hashes, .. } => {
                if frame.pos < child_hashes.len() {
                    Some(child_hashes[frame.pos])
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Skip past the pending subtree without loading. Errors if the cursor
    /// isn't in a pending-descent state.
    pub fn skip_next_subtree(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        let exhausted_internal = {
            let frame = self
                .stack
                .last_mut()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty stack"))?;
            match &frame.node.kind {
                NodeKind::Internal { child_hashes, .. } => {
                    if frame.pos >= child_hashes.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "no pending subtree",
                        ));
                    }
                    frame.pos += 1;
                    frame.pos >= child_hashes.len()
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "skip requires pending internal frame",
                    ))
                }
            }
        };
        if exhausted_internal {
            self.stack.pop();
            self.bump_parent()?;
        }
        Ok(())
    }

    /// Materialize the cursor: descend until top of stack is a leaf with
    /// `pos < entries.len()`, or the tree is exhausted.
    fn realize(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        loop {
            enum Step {
                Done,
                Ready,
                Descend(Hash),
                Bubble,
            }
            let step = match self.stack.last() {
                None => Step::Done,
                Some(frame) => match &frame.node.kind {
                    NodeKind::Leaf { entries } => {
                        if frame.pos < entries.len() {
                            Step::Ready
                        } else {
                            Step::Bubble
                        }
                    }
                    NodeKind::Internal { child_hashes, .. } => {
                        if frame.pos < child_hashes.len() {
                            Step::Descend(child_hashes[frame.pos])
                        } else {
                            Step::Bubble
                        }
                    }
                },
            };
            match step {
                Step::Done => {
                    self.done = true;
                    return Ok(());
                }
                Step::Ready => return Ok(()),
                Step::Descend(h) => {
                    let child = load_node(self.store, &h)?;
                    self.stack.push(Frame {
                        node: child,
                        pos: 0,
                    });
                }
                Step::Bubble => {
                    self.stack.pop();
                    self.bump_parent()?;
                }
            }
        }
    }

    /// After popping the top frame, advance the new top's pos by 1 (or mark done).
    fn bump_parent(&mut self) -> io::Result<()> {
        match self.stack.last_mut() {
            None => {
                self.done = true;
            }
            Some(frame) => {
                frame.pos += 1;
            }
        }
        Ok(())
    }

    /// Used by seek when a leaf was exhausted: pop already done by caller,
    /// then bump parent and descend leftmost into next sibling.
    fn bump_parent_then_descend_leftmost(&mut self) -> io::Result<()> {
        loop {
            let frame = match self.stack.last_mut() {
                None => {
                    self.done = true;
                    return Ok(());
                }
                Some(f) => f,
            };
            frame.pos += 1;
            let next_hash = match &frame.node.kind {
                NodeKind::Internal { child_hashes, .. } => {
                    if frame.pos >= child_hashes.len() {
                        None
                    } else {
                        Some(child_hashes[frame.pos])
                    }
                }
                _ => unreachable!(),
            };
            match next_hash {
                None => {
                    self.stack.pop();
                    continue;
                }
                Some(h) => {
                    let child = load_node(self.store, &h)?;
                    self.stack.push(Frame {
                        node: child,
                        pos: 0,
                    });
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ops::{bulk_build, empty, put};
    use super::*;
    use crate::store::MemStore;

    fn entries(n: usize) -> Vec<(Vec<u8>, Value)> {
        (0..n)
            .map(|i| {
                (
                    format!("key-{:08}", i).into_bytes(),
                    Value::Inline(format!("v-{}", i).into_bytes()),
                )
            })
            .collect()
    }

    #[test]
    fn open_empty_is_done() {
        let s = MemStore::new();
        let mut c = Cursor::open(&s, empty()).unwrap();
        assert!(c.done());
        assert!(c.current().unwrap().is_none());
    }

    #[test]
    fn iterate_single_leaf() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(5)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        let mut keys = Vec::new();
        while let Some((k, _)) = c.next().unwrap() {
            keys.push(k);
        }
        assert_eq!(keys.len(), 5);
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(*k, format!("key-{:08}", i).into_bytes());
        }
    }

    #[test]
    fn iterate_multilevel() {
        let s = MemStore::new();
        let n = 5000;
        let root = bulk_build(&s, entries(n)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        let mut count = 0;
        let mut last: Option<Vec<u8>> = None;
        while let Some((k, _)) = c.next().unwrap() {
            if let Some(prev) = &last {
                assert!(k.as_slice() > prev.as_slice());
            }
            last = Some(k);
            count += 1;
        }
        assert_eq!(count, n);
    }

    #[test]
    fn seek_exact_match() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(1000)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        c.seek(b"key-00000500").unwrap();
        let (k, _) = c.current().unwrap().unwrap();
        assert_eq!(k, b"key-00000500");
    }

    #[test]
    fn seek_to_first_geq() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(1000)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        c.seek(b"key-000004999").unwrap();
        let (k, _) = c.current().unwrap().unwrap();
        assert_eq!(k, b"key-00000500");
    }

    #[test]
    fn seek_past_end_is_done() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(100)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        c.seek(b"zzzzz").unwrap();
        assert!(c.done());
        assert!(c.current().unwrap().is_none());
    }

    #[test]
    fn seek_before_first_lands_on_first() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(50)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        c.seek(b"").unwrap();
        let (k, _) = c.current().unwrap().unwrap();
        assert_eq!(k, b"key-00000000");
    }

    #[test]
    fn seek_then_iterate_remainder() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(2000)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        c.seek(b"key-00001500").unwrap();
        let mut count = 0;
        while c.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 500);
    }

    #[test]
    fn seek_on_empty_is_done() {
        let s = MemStore::new();
        let mut c = Cursor::open(&s, empty()).unwrap();
        c.seek(b"anything").unwrap();
        assert!(c.done());
    }

    #[test]
    fn cursor_after_put() {
        let s = MemStore::new();
        let mut root = empty();
        for i in 0..50 {
            let k = format!("k-{:04}", i).into_bytes();
            root = put(&s, &root, k, Value::Inline(b"v".to_vec())).unwrap();
        }
        let mut c = Cursor::open(&s, root).unwrap();
        let mut count = 0;
        while c.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn skip_next_subtree_advances_without_loading() {
        let s = MemStore::new();
        let root = bulk_build(&s, entries(2000)).unwrap();
        let mut c = Cursor::open(&s, root).unwrap();
        // After open, top of stack is root (internal). pending_subtree gives first child.
        assert!(c.peek_next_subtree().is_some());
        c.skip_next_subtree().unwrap();
        // Now positioned at second child of root.
        // current() will descend into the new subtree.
        let (k, _) = c.current().unwrap().unwrap();
        // The first child contained the smallest keys, so after skipping it
        // we should be past those keys.
        assert!(k > b"key-00000000".as_slice());
    }
}
