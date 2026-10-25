# fuck-npm

A fast, disk-efficient package manager for Node.js. Bins: `fnpm` (package manager) and `fnpm-lock` (lockfile toolkit).

- Rust core (resolution, network, store, linking) loaded through Node-API.
- Global content-addressable store; projects get files via copy-on-write clones or hardlinks.
- Reads and migrates npm, yarn (v1 and berry), pnpm and bun lockfiles.
- Workspaces, catalogs, overrides, patches, lifecycle scripts.

Status: under active development. Feature parity with bun's package manager is tracked in [docs/PARITY.md](docs/PARITY.md).
