//! A small streaming tar reader for npm tarballs (ustar, GNU long names, pax `path`).
//!
//! Entries are relativised: the first path component (`package/` by convention, any name
//! accepted) is stripped, `..` / absolute / drive paths are rejected, and only regular files
//! and directories are kept (symlinks and hardlinks are skipped like npm).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryHeader {
    pub path: String,
    pub kind: EntryKind,
    pub mode: u32,
    pub size: u64,
}

/// Push parser: feed decompressed bytes as they arrive, receive entries and file bytes.
pub struct TarParser {}

pub enum TarEvent<'a> {
    Entry(EntryHeader),
    Data(&'a [u8]),
    EntryEnd,
}

impl TarParser {
    pub fn new() -> Self {
        Self {}
    }

    /// Consumes `input`, calling `on_event` for every header, data chunk and entry end.
    pub fn feed(&mut self, _input: &[u8], _on_event: &mut dyn FnMut(TarEvent<'_>) -> crate::Result<()>) -> crate::Result<()> {
        todo!()
    }

    /// Errors if the archive ended inside an entry.
    pub fn finish(self) -> crate::Result<()> {
        todo!()
    }
}

impl Default for TarParser {
    fn default() -> Self {
        Self::new()
    }
}

/// `None` for an unsafe entry path.
pub fn sanitize_entry_path(_raw: &str) -> Option<String> {
    todo!()
}
