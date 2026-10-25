//! `fnpm pm scan` (bun `runtime/cli/scan_command.rs`): the security scanner over every
//! package of the lockfile.

use serde_json::json;

use super::{Ctx, exit, ok};
use crate::security;
use crate::{CoreResult, Error, Result};

const NOT_CONFIGURED: &str = "\
To use 'fnpm pm scan', configure a security scanner in bunfig.toml:
  [install.security]
  scanner = \"package_name\"

Security scanners can be npm packages that export a scanner object.";

pub(crate) fn scan(cx: &Ctx) -> Result<CoreResult> {
    let project = cx.project()?;
    if project.config.security_scanner.is_none() {
        eprintln!("error: no security scanner configured");
        return Ok(exit(
            1,
            json!({ "advisories": [] }),
            NOT_CONFIGURED.lines().map(str::to_owned).collect(),
        ));
    }
    let (project, _ws, graph) = match cx.load() {
        Ok((p, ws, Some(g))) => (p, ws, g),
        Ok(_) => {
            eprintln!("error: missing lockfile, nothing to scan");
            eprintln!("note: run 'fnpm install' first");
            return Err(Error::Reported);
        }
        Err(e) => return Err(e),
    };
    let results = security::scan(&security::Scan {
        config: &project.config,
        root: &project.root_dir,
        graph: &graph,
        seeds: None,
        env: &project.env,
    })?;
    let data = json!({
        "advisories": results.advisories.iter().map(|a| json!({
            "package": a.package,
            "level": match a.level { security::Level::Fatal => "fatal", security::Level::Warn => "warn" },
            "description": a.description,
            "url": a.url,
        })).collect::<Vec<_>>(),
    });
    if results.advisories.is_empty() {
        return Ok(ok(data, vec!["No advisories found".to_owned()]));
    }
    let color = std::io::IsTerminal::is_terminal(&std::io::stdout())
        && std::env::var_os("NO_COLOR").is_none();
    let text = security::format_advisories(&graph, &results, color);
    let lines = text
        .strip_suffix('\n')
        .unwrap_or(&text)
        .split('\n')
        .map(str::to_owned)
        .collect();
    Ok(exit(1, data, lines))
}
