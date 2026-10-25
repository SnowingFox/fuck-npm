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
    let pj = fnpm_workspace::PackageJson::read(&dir.join("package.json"))?;
    Ok((dir, pj))
}

/// `fnpm link` registers the current package; `fnpm link <names>` installs registered ones.
pub(crate) async fn link(cx: &Ctx) -> Result<CoreResult> {
    let links = links_dir(cx)?;
    if cx.req.positionals.is_empty() {
        let (dir, pj) = package_here(&cx.req.cwd)?;
        let Some(name) = pj.name.clone().filter(|n| !n.is_empty()) else {
            return Err(Error::usage(
                "package.json missing \"name\" in this directory",
            ));
        };
        checked_name(&name)?;
        for link in registrations(&links, &name) {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent).map_err(Error::io(parent))?;
            }
            remove_link(&link);
            fnpm_store::fsops::symlink_dir(&dir, &link)
                .map_err(|e| Error::usage(format!("{}: {e}", link.display())))?;
        }
        let link = registrations(&links, &name)[0].clone();
        if !pj.bin.is_none() {
            let project = cx.project_global()?;
            let bin_dir = project.config.global_bin_dir.clone();
            std::fs::create_dir_all(&bin_dir).map_err(Error::io(&bin_dir))?;
            let mut bins = BinLinker::global(&bin_dir, &links);
            bins.link(
                &name,
                &pj.bin,
                &link,
                BinOptions {
                    refresh: true,
                    ..BinOptions::default()
                },
            )?;
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
        return Ok(ok(json!({ "registered": name, "path": dir }), lines));
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
    let links = links_dir(cx)?;
    if cx.req.positionals.is_empty() {
        let (dir, pj) = package_here(&cx.req.cwd)?;
        let Some(name) = pj.name.clone().filter(|n| !n.is_empty()) else {
            return Err(Error::usage(
                "package.json missing \"name\" in this directory",
            ));
        };
        checked_name(&name)?;
        let link = fnpm_linker::fsutil::join_name(&links, &name);
        if std::fs::symlink_metadata(&link).is_err() {
            return Err(Error::usage(format!("package \"{name}\" is not linked")));
        }
        if !pj.bin.is_none() {
            let project = cx.project_global()?;
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
