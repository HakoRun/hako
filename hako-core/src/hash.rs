use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(data: &[u8]) -> Self {
        Hash(*blake3::hash(data).as_bytes())
    }

    pub fn zero() -> Self {
        Hash([0; 32])
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for i in 0..32 {
            bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Hash(bytes))
    }
}

impl std::fmt::Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hash({}…)", &self.to_hex()[..12])
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(Hash::of(b"abc"), Hash::of(b"abc"));
        assert_ne!(Hash::of(b"abc"), Hash::of(b"abd"));
    }

    #[test]
    fn hex_roundtrip() {
        let h = Hash::of(b"test");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Hash::from_hex(&hex), Some(h));
        assert!(Hash::from_hex("not hex").is_none());
        assert!(Hash::from_hex(&"x".repeat(64)).is_none());
    }
}
