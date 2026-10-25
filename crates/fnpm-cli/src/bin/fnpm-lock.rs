//! The native `fnpm-lock` executable: answers `--version`, hands everything else to the TS
//! CLI. See `fnpm_cli` and `docs/ARCHITECTURE.md#fnpm-cli`.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    std::process::exit(fnpm_cli::main(fnpm_cli::Bin::FnpmLock));
}
