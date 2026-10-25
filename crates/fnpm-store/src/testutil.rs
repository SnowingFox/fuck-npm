//! Tarball builders shared by the unit tests (ports of the helpers in bun's
//! `bun-install-streaming-extract.test.ts`).

use std::io::Write;

pub(crate) fn octal(n: u64, width: usize) -> Vec<u8> {
    let mut s = format!("{:0w$o}", n, w = width - 1).into_bytes();
    s.push(0);
    s
}

/// A ustar header (`ustar\0` + `00`), mode 0o644, with `change` applied before the checksum.
pub(crate) fn header_with(
    name: &[u8],
    size: u64,
    typeflag: u8,
    change: impl FnOnce(&mut [u8; 512]),
) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    h[100..108].copy_from_slice(&octal(0o644, 8));
    h[108..116].copy_from_slice(&octal(0, 8));
    h[116..124].copy_from_slice(&octal(0, 8));
    h[124..136].copy_from_slice(&octal(size, 12));
    h[136..148].copy_from_slice(&octal(0, 12));
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    change(&mut h);
    seal(&mut h);
    h
}

/// Recomputes the checksum of `h`.
pub(crate) fn seal(h: &mut [u8; 512]) {
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    h[148..156].copy_from_slice(&octal(sum, 8));
}

pub(crate) fn header(name: &str, size: u64, typeflag: u8) -> [u8; 512] {
    header_with(name.as_bytes(), size, typeflag, |_| {})
}

pub(crate) fn pad(len: usize) -> Vec<u8> {
    vec![0; (512 - len % 512) % 512]
}

pub(crate) fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let mut len = 0usize;
    loop {
        let mut rec = format!("{len} {key}=").into_bytes();
        rec.extend_from_slice(value);
        rec.push(b'\n');
        if rec.len() == len {
            return rec;
        }
        len = rec.len();
    }
}

#[derive(Default)]
pub(crate) struct TarBuilder {
    pub(crate) buf: Vec<u8>,
}

impl TarBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// A header, its body and padding.
    pub(crate) fn entry(&mut self, h: [u8; 512], body: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(&h);
        self.buf.extend_from_slice(body);
        self.buf.extend_from_slice(&pad(body.len()));
        self
    }

    /// A regular file; names over 100 bytes get a pax `path` record, like npm's `tar`.
    pub(crate) fn file(&mut self, name: &str, body: &[u8]) -> &mut Self {
        if name.len() > 100 {
            let pax = pax_record("path", name.as_bytes());
            self.entry(header("PaxHeader", pax.len() as u64, b'x'), &pax);
            return self.entry(header(&name[..99], body.len() as u64, b'0'), body);
        }
        self.entry(header(name, body.len() as u64, b'0'), body)
    }

    pub(crate) fn file_mode(&mut self, name: &str, body: &[u8], mode: u32) -> &mut Self {
        let h = header_with(name.as_bytes(), body.len() as u64, b'0', |h| {
            h[100..108].copy_from_slice(&octal(u64::from(mode), 8));
        });
        self.entry(h, body)
    }

    pub(crate) fn end(&mut self) -> &mut Self {
        self.buf.extend_from_slice(&[0u8; 1024]);
        self
    }

    pub(crate) fn tar(&self) -> Vec<u8> {
        self.buf.clone()
    }

    pub(crate) fn tgz(&self) -> Vec<u8> {
        gzip(&self.buf)
    }
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).expect("in-memory write");
    enc.finish().expect("in-memory write")
}

/// Bytes gzip cannot collapse (xorshift32), the same on every run.
pub(crate) fn incompressible(bytes: usize, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    let mut x = seed.max(1);
    while out.len() < bytes {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(bytes);
    out
}
