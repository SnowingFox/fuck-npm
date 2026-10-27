//! After the queue drains: optional peers, peer warnings, collapsing appended duplicates,
//! dropping unreachable packages and assembling the output.

use fnpm_types::{PackageId, Resolution};

use super::{Engine, EngineOutput, Kind, Row};
use crate::Result;

impl Engine<'_> {
    /// Optional peers are never installed; they bind to what the declarer or the packages above
    /// it provide, and stay unresolved otherwise.
    fn bind_optional_peers(&mut self) {
        self.ensure_parents();
        for i in 0..self.graph.len() {
            let pkg = PackageId(i as u32);
            for j in 0..self.graph.packages[i].dependencies.len() {
                let e = &self.graph.packages[i].dependencies[j];
                if !e.behavior.is_optional_peer() || e.target.is_some() {
                    continue;
                }
                let name = e.name.clone();
                let row = Row {
                    pkg,
                    edge: j as u32,
                    install_peer: true,
                };
                if let Some(t) = self.peer_provided_from_above(&name, row) {
                    self.graph.packages[i].dependencies[j].target = Some(t);
                    if let Some(parents) = &mut self.parents {
                        parents[t.index()].push((pkg, true));
                    }
                }
            }
        }
    }

    /// With `auto_install_peers` off, a required peer nobody provides is reported.
    fn warn_missing_peers(&mut self) {
        if self.cfg.auto_install_peers {
            return;
        }
        let mut warnings = Vec::new();
        for p in &self.graph.packages {
            for e in &p.dependencies {
                if e.behavior.is_peer() && !e.behavior.is_optional_peer() && e.target.is_none() {
                    warnings.push(format!(
                        "missing peer dependency \"{}@{}\" of \"{}@{}\"",
                        e.name, e.literal, p.name, p.resolution
                    ));
                }
            }
        }
        self.warnings.extend(warnings);
    }

    /// Streams every package the fetch phase needs that was not streamed while resolving: a
    /// package first reached inside a bundle and later shared by a regular dependency (bun
    /// `get_package_id_outside_bundles` turns it into a regular package), or anything whose
    /// fetch state only became known at the end. Without this the fetcher would never hear of it.
    fn emit_missed(&self, map: &[Option<PackageId>]) {
        let Some(tx) = &self.tx else { return };
        let mut emitted = vec![false; self.graph.len()];
        for (old, new) in map.iter().enumerate() {
            if let Some(new) = new
                && self.st[old].emitted
            {
                emitted[new.index()] = true;
            }
        }
        for (i, keep) in crate::frozen::needed(&self.graph, self.cfg)
            .into_iter()
            .enumerate()
        {
            let p = &self.graph.packages[i];
            if keep && !emitted[i] && p.resolution.is_storable() && !p.meta.bundled {
                let _ = tx.send(crate::Resolved::of(PackageId(i as u32), p));
            }
        }
    }

    /// bun's `deprecated name@version: message`, once per version, for the versions a
    /// package.json of the project depends on (every one with `--verbose`).
    fn warn_deprecated(&mut self) {
        if self.deprecated.is_empty() {
            return;
        }
        let verbose = self.cfg.log_level == fnpm_config::LogLevel::Verbose;
        let mut direct = vec![false; self.graph.len()];
        for (i, p) in self.graph.packages.iter().enumerate() {
            if self.st.get(i).is_none_or(|s| s.kind != Kind::Project) {
                continue;
            }
            for e in &p.dependencies {
                if let Some(t) = e.target
                    && let Some(d) = direct.get_mut(t.index())
                {
                    *d = true;
                }
            }
        }
        for (id, line) in std::mem::take(&mut self.deprecated) {
            if verbose || direct.get(id.index()).copied().unwrap_or(false) {
                self.warnings.push(line);
            }
        }
    }

    pub(crate) fn finish(mut self) -> Result<EngineOutput> {
        self.bind_optional_peers();
        self.parents = None;
        let loaded: Vec<bool> = self
            .st
            .iter()
            .map(|s| s.loaded || (s.kind == Kind::Project && self.prev.is_some()))
            .collect();
        self.warn_deprecated();
        let _ = crate::dedupe::collapse_appended(&mut self.graph, &loaded);
        self.warn_missing_peers();
        let map = crate::dedupe::prune_unreachable(&mut self.graph);
        // Bundled: everything only reachable through a bundled edge of a published package.
        let mut outside = vec![false; self.graph.len()];
        let mut stack: Vec<PackageId> = self.graph.workspaces.values().copied().collect();
        while let Some(id) = stack.pop() {
            if std::mem::replace(&mut outside[id.index()], true) {
                continue;
            }
            let p = &self.graph.packages[id.index()];
            let local = p.resolution.is_local_tree();
            for e in &p.dependencies {
                if let Some(t) = e.target
                    && (local || !e.behavior.is_bundled())
                    && !outside[t.index()]
                {
                    stack.push(t);
                }
            }
        }
        for (p, out) in self.graph.packages.iter_mut().zip(&outside) {
            p.meta.bundled = !out
                && !matches!(
                    p.resolution,
                    Resolution::Root | Resolution::Workspace { .. }
                );
        }
        self.graph.sort_dependencies();
        self.emit_missed(&map);
        let requests: Vec<_> = std::mem::take(&mut self.requests)
            .into_iter()
            .zip(&self.request_targets)
            .filter_map(|(r, t)| t.and_then(|t| map[t.index()]).map(|t| (r, t)))
            .collect();
        let mut warnings = std::mem::take(&mut self.warnings);
        warnings.extend(std::mem::take(&mut self.kept_patched));
        let mut seen = std::collections::HashSet::new();
        warnings.retain(|w| seen.insert(w.clone()));
        Ok(EngineOutput {
            graph: self.graph,
            requests,
            warnings,
            manifest_order: self.manifest_order,
            unsaved: self.unsaved,
        })
    }
}
