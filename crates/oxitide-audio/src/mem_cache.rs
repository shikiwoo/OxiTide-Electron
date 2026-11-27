//! The Memory cache backend: track bytes held in RAM only.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use symphonia::core::io::MediaSource;

/// Size of one storage chunk. Large enough that the allocator hands each
/// chunk straight to the OS (and returns it when the entry is freed), small
/// enough that the unused tail of a track's last chunk stays cheap.
pub const CHUNK_BYTES: usize = 1 << 20;

#[derive(Default)]
struct Chunks {
    chunks: Vec<Box<[u8]>>,
    len: u64,
}

/// An append-only byte buffer stored in fixed-size chunks, shared between one
/// writer and any number of readers. Growing it never moves bytes already
/// written, so a track never needs twice its size in RAM.
#[derive(Default)]
pub struct ChunkBuf {
    inner: RwLock<Chunks>,
}

impl ChunkBuf {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // The data is plain bytes, so a panic elsewhere cannot leave it in a
    // state worse than "shorter than expected": recover from poisoning.
    fn read_guard(&self) -> RwLockReadGuard<'_, Chunks> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_guard(&self) -> RwLockWriteGuard<'_, Chunks> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Appends `data`, filling the last chunk before allocating a new one.
    pub fn append(&self, mut data: &[u8]) {
        let mut state = self.write_guard();
        while !data.is_empty() {
            let index = (state.len / CHUNK_BYTES as u64) as usize;
            let offset = (state.len % CHUNK_BYTES as u64) as usize;
            if index == state.chunks.len() {
                state.chunks.push(vec![0u8; CHUNK_BYTES].into_boxed_slice());
            }
            let n = (CHUNK_BYTES - offset).min(data.len());
            state.chunks[index][offset..offset + n].copy_from_slice(&data[..n]);
            state.len += n as u64;
            data = &data[n..];
        }
    }

    pub fn len(&self) -> u64 {
        self.read_guard().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies the bytes at `pos..` into `out`: at most `len - pos` of them,
    /// and 0 at or past the end. Never waits for more bytes to arrive.
    pub fn read_at(&self, pos: u64, out: &mut [u8]) -> usize {
        let state = self.read_guard();
        if pos >= state.len {
            return 0;
        }
        let total = (state.len - pos).min(out.len() as u64) as usize;
        let mut copied = 0;
        while copied < total {
            let at = pos + copied as u64;
            let index = (at / CHUNK_BYTES as u64) as usize;
            let offset = (at % CHUNK_BYTES as u64) as usize;
            let n = (CHUNK_BYTES - offset).min(total - copied);
            out[copied..copied + n].copy_from_slice(&state.chunks[index][offset..offset + n]);
            copied += n;
        }
        total
    }
}

/// A seekable reader over a [`ChunkBuf`]. It sees bytes appended after it
/// was created, and it keeps the buffer alive after the cache drops it.
pub struct ChunkReader {
    buf: Arc<ChunkBuf>,
    pos: u64,
}

impl ChunkReader {
    pub fn new(buf: Arc<ChunkBuf>) -> Self {
        Self { buf, pos: 0 }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.buf.read_at(self.pos, out);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for ChunkReader {
    /// `SeekFrom::End` is relative to the buffer's current length.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (base, delta) = match pos {
            SeekFrom::Start(to) => {
                self.pos = to;
                return Ok(to);
            }
            SeekFrom::Current(delta) => (self.pos, delta),
            SeekFrom::End(delta) => (self.buf.len(), delta),
        };
        match base.checked_add_signed(delta) {
            Some(to) => {
                self.pos = to;
                Ok(to)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )),
        }
    }
}

impl MediaSource for ChunkReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.buf.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_across_chunk_boundaries_reads_back_exactly() {
        let buf = ChunkBuf::new();
        let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        for piece in data.chunks(CHUNK_BYTES / 3 + 5) {
            buf.append(piece);
        }
        assert_eq!(buf.len(), data.len() as u64);
        let mut out = Vec::new();
        ChunkReader::new(buf.clone()).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn reader_sees_bytes_appended_after_it_was_created() {
        let buf = ChunkBuf::new();
        buf.append(b"abc");
        let mut r = ChunkReader::new(buf.clone());
        let mut out = [0u8; 8];
        assert_eq!(r.read(&mut out).unwrap(), 3);
        assert_eq!(r.read(&mut out).unwrap(), 0);
        buf.append(b"de");
        assert_eq!(r.read(&mut out).unwrap(), 2);
        assert_eq!(&out[..2], b"de");
    }

    #[test]
    fn seek_from_start_current_and_end() {
        let buf = ChunkBuf::new();
        buf.append(b"0123456789");
        let mut r = ChunkReader::new(buf);
        assert_eq!(r.seek(SeekFrom::End(-3)).unwrap(), 7);
        assert_eq!(r.seek(SeekFrom::Current(-2)).unwrap(), 5);
        let mut b = [0u8; 1];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"5");
        assert!(r.seek(SeekFrom::Current(-100)).is_err());
    }

    #[test]
    fn media_source_is_seekable_and_reports_the_current_length() {
        let buf = ChunkBuf::new();
        buf.append(b"abcd");
        let r = ChunkReader::new(buf.clone());
        assert!(r.is_seekable());
        assert_eq!(r.byte_len(), Some(4));
        buf.append(b"ef");
        assert_eq!(r.byte_len(), Some(6));
    }

    #[test]
    fn read_at_copies_at_most_the_remaining_bytes() {
        let buf = ChunkBuf::new();
        assert!(buf.is_empty());
        buf.append(b"hello");
        assert!(!buf.is_empty());
        let mut out = [0u8; 8];
        assert_eq!(buf.read_at(3, &mut out), 2);
        assert_eq!(&out[..2], b"lo");
        assert_eq!(buf.read_at(5, &mut out), 0);
        assert_eq!(buf.read_at(99, &mut out), 0);
    }
}
