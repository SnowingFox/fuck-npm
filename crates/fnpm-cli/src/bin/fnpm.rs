//! The native `fnpm` executable. See `fnpm_cli` and `docs/ARCHITECTURE.md#fnpm-cli`.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    std::process::exit(fnpm_cli::main(fnpm_cli::Bin::Fnpm));
}
