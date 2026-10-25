//! gzip decoding for tarballs: a resumable multi-member inflater (zlib-rs through flate2) for
//! streamed bodies, and a libdeflate one-shot for small buffered ones (bun
//! `extract_tarball.rs`: libdeflate first when the gzip ISIZE is small, libarchive otherwise).
//!
//! As libarchive does, a gzip member may be followed by more members (they are concatenated)
//! and anything after the last member that does not start with the gzip magic is ignored.
//! Each member's CRC-32 and length are checked.

use flate2::{Decompress, FlushDecompress, Status};

use crate::{Error, Result};

const OUT_CHUNK: usize = 128 * 1024;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidTarball {
        package: String::new(),
        message: message.into(),
    }
}

/// True if `bytes` starts like a gzip stream.
pub(crate) fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&GZIP_MAGIC)
}

/// Streaming gzip decoder; decompressed bytes go to the sink as they are produced.
pub(crate) struct GzipStream {
    inflate: Decompress,
    out: Vec<u8>,
    /// The current member reached its end; the next bytes decide whether another follows.
    member_done: bool,
    /// What came after the last member is not gzip, so the rest of the input is ignored.
    trailing: bool,
    /// Bytes after a member end, held until two of them show whether a member follows.
    held: Vec<u8>,
    started: bool,
    /// The last call filled the output buffer, so zlib may hold more output.
    out_full: bool,
}

impl GzipStream {
    pub(crate) fn new() -> Self {
        Self {
            inflate: Decompress::new_gzip(15),
            out: vec![0; OUT_CHUNK],
            member_done: false,
            trailing: false,
            held: Vec::new(),
            started: false,
            out_full: false,
        }
    }

    pub(crate) fn feed(
        &mut self,
        input: &[u8],
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if !self.held.is_empty() {
            let mut joined = std::mem::take(&mut self.held);
            joined.extend_from_slice(input);
            return self.feed_inner(&joined, sink);
        }
        self.feed_inner(input, sink)
    }

    fn feed_inner(
        &mut self,
        mut input: &[u8],
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        loop {
            if self.trailing {
                return Ok(());
            }
            if input.is_empty() && !self.out_full {
                return Ok(());
            }
            if self.member_done {
                if input.is_empty() {
                    return Ok(());
                }
                if input.len() < 2 {
                    self.held.extend_from_slice(input);
                    return Ok(());
                }
                if !is_gzip(input) {
                    self.trailing = true;
                    return Ok(());
                }
                self.inflate = Decompress::new_gzip(15);
                self.member_done = false;
            }
            self.started = true;
            let before_in = self.inflate.total_in();
            let before_out = self.inflate.total_out();
            let status = self
                .inflate
                .decompress(input, &mut self.out, FlushDecompress::None)
                .map_err(|e| invalid(format!("gzip decompression failed: {e}")))?;
            let consumed = (self.inflate.total_in() - before_in) as usize;
            let produced = (self.inflate.total_out() - before_out) as usize;
            input = &input[consumed..];
            self.out_full = produced == self.out.len();
            if produced > 0 {
                sink(&self.out[..produced])?;
            }
            match status {
                Status::StreamEnd => {
                    self.member_done = true;
                    self.out_full = false;
                }
                Status::Ok | Status::BufError => {
                    if consumed == 0 && produced == 0 {
                        if input.is_empty() {
                            return Ok(());
                        }
                        return Err(invalid("gzip decompression failed: no progress"));
                    }
                }
            }
        }
    }

    /// Errors if the body ended inside a member.
    pub(crate) fn finish(&mut self, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.feed_inner(&[], sink)?;
        if !self.started || self.member_done || self.trailing {
            return Ok(());
        }
        Err(invalid("gzip decompression failed: unexpected end of file"))
    }
}

/// ISIZE limits for the one-shot path (bun: `isize > 16 && isize < 64 MiB`).
const ONESHOT_MAX: u32 = 64 * 1024 * 1024;

struct Decompressor(*mut libdeflate_sys::libdeflate_decompressor);

impl Drop for Decompressor {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `libdeflate_alloc_decompressor` and is freed once.
        unsafe { libdeflate_sys::libdeflate_free_decompressor(self.0) }
    }
}

thread_local! {
    static DECOMPRESSOR: std::cell::RefCell<Option<Decompressor>> = const { std::cell::RefCell::new(None) };
    static BUFFER: std::cell::Cell<Vec<u8>> = const { std::cell::Cell::new(Vec::new()) };
}

/// Inflates a whole single-member gzip body in one call when its ISIZE trailer says it is
/// small. `None` when the fast path does not apply (large, multi-member, trailing bytes,
/// corrupt): the caller then streams it, which also reports the precise error.
pub(crate) fn inflate_oneshot(tgz: &[u8]) -> Option<Vec<u8>> {
    if tgz.len() <= 18 || !is_gzip(tgz) {
        return None;
    }
    let isize = u32::from_le_bytes(tgz[tgz.len() - 4..].try_into().ok()?);
    if isize <= 16 || isize >= ONESHOT_MAX {
        return None;
    }
    let mut out = BUFFER.with(|b| b.take());
    out.clear();
    out.reserve_exact(isize as usize);
    let ok = DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            // SAFETY: plain allocation call; null is handled below.
            let p = unsafe { libdeflate_sys::libdeflate_alloc_decompressor() };
            if p.is_null() {
                return false;
            }
            *slot = Some(Decompressor(p));
        }
        let Some(d) = slot.as_ref() else { return false };
        let mut actual_in: usize = 0;
        let mut actual_out: usize = 0;
        // SAFETY: `tgz` is readable for its length and `out` has `isize` bytes of spare
        // capacity; libdeflate writes at most `out_nbytes_avail` bytes and reports how many.
        let result = unsafe {
            libdeflate_sys::libdeflate_gzip_decompress_ex(
                d.0,
                tgz.as_ptr().cast(),
                tgz.len(),
                out.as_mut_ptr().cast(),
                isize as usize,
                &mut actual_in,
                &mut actual_out,
            )
        };
        if result == libdeflate_sys::libdeflate_result_LIBDEFLATE_SUCCESS
            && actual_in == tgz.len()
            && actual_out == isize as usize
        {
            // SAFETY: libdeflate initialised exactly `actual_out` bytes.
            unsafe { out.set_len(actual_out) };
            true
        } else {
            false
        }
    });
    if ok {
        Some(out)
    } else {
        recycle(out);
        None
    }
}

/// Returns a buffer from [`inflate_oneshot`] for reuse by the next tarball on this thread.
pub(crate) fn recycle(mut buf: Vec<u8>) {
    // Very large buffers are not kept, so an idle thread does not pin them.
    if buf.capacity() <= 16 * 1024 * 1024 {
        buf.clear();
        BUFFER.with(|b| b.set(buf));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{gzip, incompressible};

    fn stream_all(data: &[u8], chunk: usize) -> Result<Vec<u8>> {
        let mut g = GzipStream::new();
        let mut out = Vec::new();
        for piece in data.chunks(chunk.max(1)) {
            g.feed(piece, &mut |d| {
                out.extend_from_slice(d);
                Ok(())
            })?;
        }
        g.finish(&mut |d| {
            out.extend_from_slice(d);
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn streams_in_any_chunk_size() {
        let plain = incompressible(300_000, 3);
        let gz = gzip(&plain);
        for chunk in [1, 2, 9, 10, 11, 4096, gz.len()] {
            assert_eq!(stream_all(&gz, chunk).unwrap(), plain, "chunk {chunk}");
        }
    }

    #[test]
    fn concatenated_members_and_trailing_garbage() {
        let mut gz = gzip(b"hello ");
        gz.extend(gzip(b"world"));
        assert_eq!(stream_all(&gz, 3).unwrap(), b"hello world");
        gz.extend_from_slice(b"\x00\x00trailing junk");
        assert_eq!(stream_all(&gz, 5).unwrap(), b"hello world");
        assert!(inflate_oneshot(&gz).is_none());
    }

    #[test]
    fn truncated_and_corrupt() {
        let gz = gzip(&incompressible(10_000, 1));
        assert!(stream_all(&gz[..gz.len() - 3], 100).is_err());
        let mut bad = gz.clone();
        let n = bad.len();
        bad[n - 6] ^= 0xff; // CRC
        assert!(stream_all(&bad, 100).is_err());
        assert!(inflate_oneshot(&bad).is_none());
        let mut bad = gz;
        bad[100] ^= 0x55;
        assert!(stream_all(&bad, 100).is_err());
    }

    #[test]
    fn oneshot_matches_streaming() {
        let plain = incompressible(100_000, 9);
        let gz = gzip(&plain);
        let out = inflate_oneshot(&gz).unwrap();
        assert_eq!(out, plain);
        recycle(out);
        assert_eq!(inflate_oneshot(&gz).unwrap(), plain);
        // Too small to be worth it (ISIZE <= 16).
        assert!(inflate_oneshot(&gzip(b"tiny")).is_none());
    }
}
