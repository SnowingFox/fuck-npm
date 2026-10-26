//! The `node-gyp` shim on lifecycle scripts' PATH (bun `ensure_temp_node_gyp_script`): a
//! script that runs `$npm_config_node_gyp` when set, else `fnpm x --silent node-gyp`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

#[cfg(windows)]
const FILE_NAME: &str = "node-gyp.cmd";
#[cfg(not(windows))]
const FILE_NAME: &str = "node-gyp";

#[cfg(windows)]
const CONTENT: &str = "if not defined npm_config_node_gyp (\n  fnpm x --silent node-gyp %*\n) else (\n  node \"%npm_config_node_gyp%\" %*\n)\n";
#[cfg(all(not(windows), not(target_os = "android")))]
const CONTENT: &str = "#!/bin/sh\nif [ \"x$npm_config_node_gyp\" = \"x\" ]; then\n  fnpm x --silent node-gyp $@\nelse\n  \"$npm_config_node_gyp\" $@\nfi\n";
#[cfg(target_os = "android")]
const CONTENT: &str = "#!/system/bin/sh\nif [ \"x$npm_config_node_gyp\" = \"x\" ]; then\n  fnpm x --silent node-gyp $@\nelse\n  \"$npm_config_node_gyp\" $@\nfi\n";

/// A temporary directory holding the shim; removed on drop.
#[derive(Debug)]
pub struct NodeGypShim {
    dir: PathBuf,
}

impl NodeGypShim {
    /// Creates `<tmp>/fnpm-node-gyp-<random>/node-gyp`. The name is random so that another
    /// user cannot plant a directory there first; creation fails if it exists.
    pub fn create(tmp: &Path) -> std::io::Result<Self> {
        let mut last_err = None;
        for attempt in 0..8u32 {
            let dir = tmp.join(format!("fnpm-node-gyp-{:016x}", random_u64(attempt)));
            match create_private_dir(&dir) {
                Ok(()) => {
                    let shim = Self { dir };
                    shim.write_script()?;
                    return Ok(shim);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("could not create node-gyp tempdir")))
    }

    /// The directory to put on PATH.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn write_script(&self) -> std::io::Result<()> {
        let path = self.dir.join(FILE_NAME);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o755);
        }
        let mut file = options.open(&path)?;
        file.write_all(CONTENT.as_bytes())
    }
}

impl Drop for NodeGypShim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn random_u64(salt: u32) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(salt);
    h.write_u32(std::process::id());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    h.write_u128(nanos);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_random_dir_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let a = NodeGypShim::create(tmp.path()).unwrap();
        let b = NodeGypShim::create(tmp.path()).unwrap();
        assert_ne!(a.dir(), b.dir());
        let script = std::fs::read_to_string(a.dir().join(FILE_NAME)).unwrap();
        assert!(script.contains("fnpm x --silent node-gyp"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(a.dir().join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
            let dir_mode = std::fs::metadata(a.dir()).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o077, 0);
        }
        let dir = a.dir().to_path_buf();
        drop(a);
        assert!(!dir.exists());
    }
}
