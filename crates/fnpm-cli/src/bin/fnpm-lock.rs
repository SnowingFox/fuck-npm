//! The native `fnpm-lock` executable: answers `--version`, hands everything else to the TS
//! CLI. See `fnpm_cli` and `docs/ARCHITECTURE.md#fnpm-cli`.

fnpm_cli::native_allocator!();

fn main() {
    fnpm_cli::exit(fnpm_cli::main_lock());
}
