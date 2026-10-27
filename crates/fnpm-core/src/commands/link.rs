//! `fnpm link` / `fnpm unlink` (bun `link_command.rs`, `unlink_command.rs`): packages are
//! registered as symlinks in `<global dir>/links/<name>` (where the linker points
//! `link:<name>` dependencies) and in `<global dir>/node_modules/<name>` (bun's location, which
//! the resolver checks).

use std::path::{Path, PathBuf};

use fnpm_linker::bins::{BinLinker, BinOptions};
use serde_json::json;

use super::{Ctx, ok};
use crate::options::{InstallCommand, InstallOptions};
use crate::{CoreResult, Error, Result};

fn links_dir(cx: &Ctx) -> Result<PathBuf> {
    Ok(cx.project_global()?.config.global_dir.join("links"))
}

/// Both registration paths of `name`.
fn registrations(links: &Path, name: &str) -> [PathBuf; 2] {
    let global = links.parent().unwrap_or(links).join("node_modules");
    [
        fnpm_linker::fsutil::join_name(links, name),
        fnpm_linker::fsutil::join_name(&global, name),
    ]
}

/// A name that becomes a path under the link registry must be a valid package name (no `..`,
/// no absolute path), or `fnpm link` could replace or delete anything on disk.
fn checked_name(name: &str) -> Result<&str> {
    if fnpm_types::name::is_npm_package_name(name) {
        Ok(name)
    } else {
        Err(Error::usage(format!(
            "\"{name}\" is not a valid package name"
        )))
    }
}

fn remove_link(p: &Path) {
    if std::fs::symlink_metadata(p).is_ok() {
        let _ = std::fs::remove_file(p).or_else(|_| std::fs::remove_dir_all(p));
    }
}

impl Ctx {
    /// The configuration without a project (link registration is global).
    fn project_global(&self) -> Result<crate::project::Project> {
        match self.project() {
            Ok(p) => Ok(p),
            Err(Error::Workspace(fnpm_workspace::Error::NoProject(_))) => {
                let mut cli = self.pm.to_cli_overrides();
                cli.global = true;
                crate::project::Project::load(&self.req.cwd, cli, self.env.clone(), false)
            }
            Err(e) => Err(e),
        }
    }
}

fn package_here(cwd: &Path) -> Result<(PathBuf, fnpm_workspace::PackageJson)> {
    let dir = fnpm_workspace::Workspace::find_root(cwd)
        .ok()
        .filter(|d| d.join("package.json").exists())
        .map(|_| {
            // The nearest package.json, not the workspace root.
            let mut d = Some(cwd);
            while let Some(x) = d {
                if x.join("package.json").is_file() {
                    return x.to_path_buf();
                }
                d = x.parent();
            }
            cwd.to_path_buf()
        })
        .unwrap_or_else(|| cwd.to_path_buf());
    let file = dir.join("package.json");
    if !file.is_file() {
        return Err(Error::usage(
            "fnpm could not find a package.json file to install from\nnote: Run \"fnpm init\" to initialize a project",
        ));
    }
    let pj = fnpm_workspace::PackageJson::read(&file)?;
    Ok((dir, pj))
}

/// The name of the package in `dir`, which `fnpm link` / `fnpm unlink` register under: bun
/// refuses a package.json without one, or with one that is not a valid package name.
fn own_name(dir: &Path, pj: &fnpm_workspace::PackageJson) -> Result<String> {
    let file = dir.join("package.json");
    match pj.name.clone().filter(|n| !n.is_empty()) {
        None => Err(Error::usage(format!(
            "package.json missing \"name\" in \"{}\"",
            file.display()
        ))),
        Some(name) if !fnpm_types::name::is_npm_package_name(&name) => Err(Error::usage(format!(
            "invalid package.json name \"{}\" in \"{}\"",
            super::pack::out::escape_control(&name),
            file.display()
        ))),
        Some(name) => Ok(name),
    }
}

/// `fnpm link` registers the current package; `fnpm link <names>` installs registered ones.
pub(crate) async fn link(cx: &Ctx) -> Result<CoreResult> {
    // The package here is checked first: bun says that a package.json is missing before it
    // looks at the global directory.
    let here = if cx.req.positionals.is_empty() {
        Some(package_here(&cx.req.cwd)?)
    } else {
        None
    };
    let links = links_dir(cx)?;
    if let Some((dir, pj)) = here {
        let name = own_name(&dir, &pj)?;
        // The relative link target is computed between the real paths: with a symlinked home
        // (`/home/u -> /data/home/u`) one from the spelled paths points elsewhere.
        let target = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        for link in registrations(&links, &name) {
            let mut at = link.clone();
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent).map_err(Error::io(parent))?;
                if let (Ok(real), Some(file)) = (std::fs::canonicalize(parent), link.file_name()) {
                    at = real.join(file);
                }
            }
            remove_link(&at);
            fnpm_store::fsops::symlink_dir(&target, &at)
                .map_err(|e| Error::usage(format!("{}: {e}", link.display())))?;
        }
        // The bins go through `<global dir>/node_modules/<name>`, where a global install puts
        // packages, so `fnpm link` and `fnpm add -g` own the same bin names (bun).
        let global = registrations(&links, &name)[1].clone();
        let mut failed = false;
        if !pj.bin.is_none() {
            let project = cx.project_global()?;
            let bin_dir = project.config.global_bin_dir.clone();
            std::fs::create_dir_all(&bin_dir).map_err(Error::io(&bin_dir))?;
            let owner = global.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut bins = BinLinker::global(&bin_dir, &owner);
            if let Err(e) = bins.link(
                &name,
                &pj.bin,
                &global,
                BinOptions {
                    refresh: true,
                    ..BinOptions::default()
                },
            ) {
                let code = match &e {
                    fnpm_linker::Error::Io { source, .. } => super::pack::out::errno_name(source),
                    other => other.to_string(),
                };
                cx.events.emit(fnpm_types::Event::Error {
                    message: format!("failed to link bin due to error {code}"),
                    code: Some("BIN_LINK_FAILED".into()),
                });
                failed = true;
            }
        }
        let lines = vec![
            format!("Success! Registered \"{name}\""),
            String::new(),
            format!("To use {name} in a project, run:"),
            format!("  fnpm link {name}"),
            String::new(),
            "Or add it in dependencies in your package.json file:".into(),
            format!("  \"{name}\": \"link:{name}\""),
        ];
        let data = json!({ "registered": name, "path": dir });
        if failed {
            return Ok(super::exit(1, data, lines));
        }
        return Ok(ok(data, lines));
    }
    let mut packages = Vec::new();
    for name in &cx.req.positionals {
        let name = checked_name(name)?;
        let link = fnpm_linker::fsutil::join_name(&links, name);
        if std::fs::symlink_metadata(&link).is_err() {
            return Err(Error::usage(format!("package \"{name}\" is not linked")));
        }
        packages.push(format!("{name}@link:{name}"));
    }
    let opts = InstallOptions {
        pm: cx.pm.clone(),
        command: InstallCommand::Add,
        packages,
        ..InstallOptions::default()
    };
    let summary = crate::install::run(opts, cx.events.clone(), cx.env.clone()).await?;
    let lines = cx
        .req
        .positionals
        .iter()
        .map(|n| format!("installed {n}@link:{n}"))
        .collect();
    Ok(ok(
        serde_json::to_value(&summary).unwrap_or_default(),
        lines,
    ))
}

/// `fnpm unlink` unregisters the current package; `fnpm unlink <names>` removes linked
/// packages from the project.
pub(crate) async fn unlink(cx: &Ctx) -> Result<CoreResult> {
    // The package here is checked first: bun says that a package.json is missing before it
    // looks at the global directory.
    let here = if cx.req.positionals.is_empty() {
        Some(package_here(&cx.req.cwd)?)
    } else {
        None
    };
    let links = links_dir(cx)?;
    if let Some((dir, pj)) = here {
        let name = own_name(&dir, &pj)?;
        let link = fnpm_linker::fsutil::join_name(&links, &name);
        if std::fs::symlink_metadata(&link).is_err() {
            return Err(Error::usage(format!("package \"{name}\" is not linked")));
        }
        if !pj.bin.is_none() {
            let project = cx.project_global()?;
            let global = registrations(&links, &name)[1].clone();
            let _ = fnpm_linker::bins::unlink_bins(&global, &project.config.global_bin_dir);
            let _ = fnpm_linker::bins::unlink_bins(&link, &project.config.global_bin_dir);
        }
        for r in registrations(&links, &name) {
            remove_link(&r);
        }
        let _ = dir;
        return Ok(ok(
            json!({ "unregistered": name }),
            vec![format!("success: unlinked package \"{name}\"")],
        ));
    }
    let opts = InstallOptions {
        pm: cx.pm.clone(),
        command: InstallCommand::Remove,
        packages: cx.req.positionals.clone(),
        ..InstallOptions::default()
    };
    let summary = crate::install::run(opts, cx.events.clone(), cx.env.clone()).await?;
    Ok(ok(
        serde_json::to_value(&summary).unwrap_or_default(),
        Vec::new(),
    ))
}
