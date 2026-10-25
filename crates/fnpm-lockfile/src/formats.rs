//! One module per lockfile format. Every module exposes `parse` and, where writable,
//! `serialize`, both pure functions over bytes / strings.

pub mod bun_binary;
pub mod bun_text;
pub mod fnpm;
pub mod foreign;
pub mod npm;
pub mod pnpm;
pub mod yarn_berry;
pub mod yarn_v1;
