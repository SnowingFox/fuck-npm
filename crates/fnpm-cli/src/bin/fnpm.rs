//! The native `fnpm` executable. See `fnpm_cli` and `docs/ARCHITECTURE.md#fnpm-cli`.

fnpm_cli::native_allocator!();

fn main() {
    fnpm_cli::exit(fnpm_cli::main(fnpm_cli::Bin::Fnpm));
}
