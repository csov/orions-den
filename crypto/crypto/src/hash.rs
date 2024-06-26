pub const SIZE: usize = 32;

pub type Hash = [u8; SIZE];

pub fn new(data: impl AsRef<[u8]>) -> Hash {
    blake3::hash(data.as_ref()).into()
}

#[derive(Default)]
pub struct HashBuffer(blake3::Hasher);

impl codec::Buffer for HashBuffer {
    fn extend_from_slice(&mut self, slice: &[u8]) -> Option<()> {
        self.0.update(slice);
        Some(())
    }

    fn push(&mut self, byte: u8) -> Option<()> {
        self.0.update(&[byte]);
        Some(())
    }
}

impl Into<Hash> for HashBuffer {
    fn into(self) -> Hash {
        self.0.finalize().into()
    }
}

#[must_use]
pub fn combine(left: Hash, right: Hash) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(left.as_ref());
    hasher.update(right.as_ref());
    hasher.finalize().into()
}

#[must_use]
pub fn with_nonce(data: &[u8], nonce: u64) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(data);
    hasher.update(&nonce.to_be_bytes());
    hasher.finalize().into()
}

pub fn kv(key: &[u8], value: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(key);
    hasher.update(value);
    hasher.finalize().into()
}

pub fn read<const BUFFER_SIZE: usize>(
    reader: &mut impl std::io::Read,
    mut len: usize,
) -> std::io::Result<Hash> {
    let mut buffer = [0; BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();
    while len > 0 {
        let red = len.min(BUFFER_SIZE);
        reader.read_exact(&mut buffer[..red])?;
        hasher.update(&buffer[..red]);
        len -= red;
    }
    Ok(hasher.finalize().into())
}
