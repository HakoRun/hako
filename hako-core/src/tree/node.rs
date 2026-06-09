use super::types::*;
use crate::hash::Hash;
use crate::store::ChunkStore;
use std::io;

/// Bumped whenever the on-disk node layout changes incompatibly.
/// Decode rejects any other value — there is no in-place migration today.
const NODE_VERSION: u8 = 1;

const TAG_INLINE: u8 = 0x00;
const TAG_EXTERNAL: u8 = 0x01;

pub fn encode(node: &Node) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(NODE_VERSION);
    buf.push(node.level);
    match &node.kind {
        NodeKind::Leaf { entries } => {
            let n = entries.len() as u16;
            buf.extend_from_slice(&n.to_be_bytes());
            for e in entries {
                let key_len = e.key.len() as u16;
                buf.extend_from_slice(&key_len.to_be_bytes());
                buf.extend_from_slice(&e.key);
                match &e.value {
                    Value::Inline(data) => {
                        buf.push(TAG_INLINE);
                        let len = data.len() as u16;
                        buf.extend_from_slice(&len.to_be_bytes());
                        buf.extend_from_slice(data);
                    }
                    Value::External(h) => {
                        buf.push(TAG_EXTERNAL);
                        buf.extend_from_slice(&h.0);
                    }
                }
            }
        }
        NodeKind::Internal {
            child_keys,
            child_hashes,
            child_counts,
        } => {
            let n = child_keys.len() as u16;
            buf.extend_from_slice(&n.to_be_bytes());
            for (k, h) in child_keys.iter().zip(child_hashes.iter()) {
                let key_len = k.len() as u16;
                buf.extend_from_slice(&key_len.to_be_bytes());
                buf.extend_from_slice(k);
                buf.extend_from_slice(&h.0);
            }
            for c in child_counts {
                buf.extend_from_slice(&c.to_be_bytes());
            }
        }
    }
    buf
}

pub fn decode(data: &[u8]) -> io::Result<Node> {
    let mut r = Reader::new(data);
    let version = r.u8()?;
    if version != NODE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unknown node version {} (this build expects {})",
                version, NODE_VERSION
            ),
        ));
    }
    let level = r.u8()?;
    let n = r.u16()? as usize;
    if level == 0 {
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let key_len = r.u16()? as usize;
            let key = r.take(key_len)?.to_vec();
            let tag = r.u8()?;
            let value = match tag {
                TAG_INLINE => {
                    let vlen = r.u16()? as usize;
                    Value::Inline(r.take(vlen)?.to_vec())
                }
                TAG_EXTERNAL => {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(r.take(32)?);
                    Value::External(Hash(h))
                }
                t => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown value tag {}", t),
                    ))
                }
            };
            entries.push(Entry { key, value });
        }
        if r.pos != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes in leaf node",
            ));
        }
        Ok(Node {
            level,
            kind: NodeKind::Leaf { entries },
        })
    } else {
        let mut child_keys = Vec::with_capacity(n);
        let mut child_hashes = Vec::with_capacity(n);
        for _ in 0..n {
            let key_len = r.u16()? as usize;
            child_keys.push(r.take(key_len)?.to_vec());
            let mut h = [0u8; 32];
            h.copy_from_slice(r.take(32)?);
            child_hashes.push(Hash(h));
        }
        let mut child_counts = Vec::with_capacity(n);
        for _ in 0..n {
            child_counts.push(r.u32()?);
        }
        if r.pos != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes in internal node",
            ));
        }
        Ok(Node {
            level,
            kind: NodeKind::Internal {
                child_keys,
                child_hashes,
                child_counts,
            },
        })
    }
}

pub fn store_node(store: &dyn ChunkStore, node: &Node) -> io::Result<Hash> {
    let bytes = encode(node);
    store.put(&bytes)
}

pub fn load_node(store: &dyn ChunkStore, hash: &Hash) -> io::Result<Node> {
    let data = store
        .get(hash)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing node"))?;
    decode(&data)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn u8(&mut self) -> io::Result<u8> {
        if self.pos + 1 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated u8"));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }
    fn u16(&mut self) -> io::Result<u16> {
        if self.pos + 2 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated u16"));
        }
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }
    fn u32(&mut self) -> io::Result<u32> {
        if self.pos + 4 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated u32"));
        }
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated take"));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    #[test]
    fn encode_decode_leaf_inline_only() {
        let n = Node {
            level: 0,
            kind: NodeKind::Leaf {
                entries: vec![
                    Entry {
                        key: b"a".to_vec(),
                        value: Value::Inline(b"1".to_vec()),
                    },
                    Entry {
                        key: b"bb".to_vec(),
                        value: Value::Inline(b"".to_vec()),
                    },
                ],
            },
        };
        assert_eq!(decode(&encode(&n)).unwrap(), n);
    }

    #[test]
    fn encode_decode_leaf_mixed_values() {
        let n = Node {
            level: 0,
            kind: NodeKind::Leaf {
                entries: vec![
                    Entry {
                        key: b"k1".to_vec(),
                        value: Value::Inline(b"short".to_vec()),
                    },
                    Entry {
                        key: b"k2".to_vec(),
                        value: Value::External(Hash::of(b"chunk")),
                    },
                ],
            },
        };
        assert_eq!(decode(&encode(&n)).unwrap(), n);
    }

    #[test]
    fn encode_decode_internal() {
        let n = Node {
            level: 1,
            kind: NodeKind::Internal {
                child_keys: vec![b"alpha".to_vec(), b"omega".to_vec()],
                child_hashes: vec![Hash::of(b"c1"), Hash::of(b"c2")],
                child_counts: vec![10, 20],
            },
        };
        assert_eq!(decode(&encode(&n)).unwrap(), n);
    }

    #[test]
    fn encode_is_deterministic() {
        let n = Node {
            level: 0,
            kind: NodeKind::Leaf {
                entries: vec![Entry {
                    key: b"k".to_vec(),
                    value: Value::Inline(b"v".to_vec()),
                }],
            },
        };
        assert_eq!(encode(&n), encode(&n));
    }

    #[test]
    fn store_load_roundtrip() {
        let s = MemStore::new();
        let n = Node {
            level: 0,
            kind: NodeKind::Leaf {
                entries: vec![Entry {
                    key: b"x".to_vec(),
                    value: Value::Inline(b"y".to_vec()),
                }],
            },
        };
        let h = store_node(&s, &n).unwrap();
        let loaded = load_node(&s, &h).unwrap();
        assert_eq!(n, loaded);
    }

    #[test]
    fn load_missing_node_errors() {
        let s = MemStore::new();
        let err = load_node(&s, &Hash::of(b"nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        // version=1, level=0, count=1, key_len=1, "k", tag=0xFF
        let bad = vec![NODE_VERSION, 0, 0, 1, 0, 1, b'k', 0xFF];
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn decode_rejects_unknown_version() {
        // version=0xFF — no such version exists.
        let bad = vec![0xFFu8, 0, 0, 0];
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let n = Node {
            level: 0,
            kind: NodeKind::Leaf {
                entries: vec![Entry {
                    key: b"k".to_vec(),
                    value: Value::Inline(b"v".to_vec()),
                }],
            },
        };
        let mut bytes = encode(&n);
        bytes.push(0xFF);
        assert!(decode(&bytes).is_err());
    }
}
