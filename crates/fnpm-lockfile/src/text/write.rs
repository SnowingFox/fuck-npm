//! The bun.lock / fnpm.lock writer (port of bun `bun.lock.rs` `Stringifier`).

use std::collections::BTreeMap;

use fnpm_types::{Behavior, Bin, CpuSet, LibcSet, LockGraph, OsSet, Package, Resolution};

use super::{LockfileVersion, url_is_under_registry};
use crate::bun_hash::{string_hash, written_order};
use crate::flat::{FDep, Flat, INVALID};
use crate::json_out::{escaped, quoted};
use crate::tree::{self, Folder, Hoisted};

const GROUPS: [(&str, Behavior); 4] = [
    ("dependencies", Behavior::PROD),
    ("devDependencies", Behavior::DEV),
    ("optionalDependencies", Behavior::OPTIONAL),
    ("peerDependencies", Behavior::PEER),
];

/// The writer's output plus the nesting cycle hoisting had to break, if any.
pub(crate) struct Written {
    pub text: String,
    pub cycle: Option<String>,
}

struct W<'a, 'g> {
    out: String,
    indent: u32,
    flat: &'a Flat<'g>,
    /// Names of every row in every folder, for `unresolved`.
    placed_names: Vec<bool>,
    any_unresolved: bool,
}

impl W<'_, '_> {
    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
    }
    fn inc_indent(&mut self) {
        self.indent += 1;
        self.write_indent();
    }
    fn dec_indent(&mut self) {
        self.indent -= 1;
        self.write_indent();
    }
    fn s(&mut self, s: &str) {
        self.out.push_str(s);
    }
    fn q(&mut self, s: &str) {
        quoted(&mut self.out, s);
    }
    fn e(&mut self, s: &str) {
        escaped(&mut self.out, s);
    }

    /// Optional dependencies that did not resolve although the tree has a package of that
    /// name; written as `unresolved` so loading does not bind them to it.
    fn unresolved_names(&self, dep_ids: &[u32]) -> Vec<String> {
        if !self.any_unresolved {
            return Vec::new();
        }
        dep_ids
            .iter()
            .filter(|&&d| self.flat.resolutions[d as usize] == INVALID)
            .map(|&d| &self.flat.deps[d as usize])
            .filter(|d| {
                d.behavior.is_optional()
                    && self
                        .placed_names
                        .get(d.name_id as usize)
                        .copied()
                        .unwrap_or(false)
            })
            .map(|d| d.name.to_owned())
            .collect()
    }
}

/// Dependency ids of a package sorted by name (ties keep bun's group order).
fn sorted_by_name(flat: &Flat<'_>, pkg: u32) -> Vec<u32> {
    let mut v: Vec<u32> = flat.list(pkg).collect();
    v.sort_by(|&a, &b| {
        flat.deps[a as usize]
            .name
            .as_bytes()
            .cmp(flat.deps[b as usize].name.as_bytes())
    });
    v
}

fn in_group(d: &FDep<'_>, group: Behavior) -> bool {
    if !d.behavior.intersects(group) {
        return false;
    }
    // Optional peers are only written under `peerDependencies`.
    !(d.behavior.is_optional_peer() && group == Behavior::OPTIONAL)
}

/// bun's `Negatable::to_json` name orders (length, then bytes).
const OS_ORDER: [&str; 8] = [
    "aix", "linux", "sunos", "win32", "darwin", "android", "freebsd", "openbsd",
];
const CPU_ORDER: [&str; 11] = [
    "arm", "ppc", "x32", "x64", "ia32", "mips", "s390", "arm64", "ppc64", "s390x", "mipsel",
];

fn negatable_json(out: &mut String, names: &[&str], has: impl Fn(&str) -> bool, none: bool) {
    if none {
        out.push_str("\"none\"");
        return;
    }
    let removed = names.iter().filter(|n| !has(n)).count();
    let included = names.len() - removed;
    let print_included = removed > included;
    let one = (print_included && included == 1) || (!print_included && removed == 1);
    if !one {
        out.push_str("[ ");
    }
    for n in names {
        let h = has(n);
        if h && print_included {
            out.push('"');
            out.push_str(n);
            out.push('"');
        } else if !h && !print_included {
            out.push_str("\"!");
            out.push_str(n);
            out.push('"');
        } else {
            continue;
        }
        if one {
            return;
        }
        out.push_str(", ");
    }
    out.push(']');
}

pub(crate) fn os_json(out: &mut String, os: OsSet) {
    negatable_json(
        out,
        &OS_ORDER,
        |n| OsSet::lookup(n).is_some_and(|b| os.contains(b)),
        os == OsSet::NONE,
    );
}

pub(crate) fn cpu_json(out: &mut String, cpu: CpuSet) {
    negatable_json(
        out,
        &CPU_ORDER,
        |n| CpuSet::lookup(n).is_some_and(|b| cpu.contains(b)),
        cpu == CpuSet::NONE,
    );
}

fn libc_single(libc: LibcSet) -> Option<&'static str> {
    match libc.0 {
        x if x == LibcSet::lookup("glibc").unwrap_or(0) => Some("glibc"),
        x if x == LibcSet::lookup("musl").unwrap_or(0) => Some("musl"),
        _ => None,
    }
}

fn bin_single_line(out: &mut String, bin: &Bin) {
    match bin {
        Bin::None => {}
        Bin::File { path } | Bin::Dir { path } => quoted(out, path),
        Bin::NamedFile { name, path } => {
            out.push_str("{ ");
            quoted(out, name);
            out.push_str(": ");
            quoted(out, path);
            out.push_str(" }");
        }
        Bin::Map { entries } => {
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                out.push(' ');
                quoted(out, k);
                out.push_str(": ");
                quoted(out, v);
            }
            out.push_str(" }");
        }
    }
}

fn bin_key(bin: &Bin) -> &'static str {
    if matches!(bin, Bin::Dir { .. }) {
        "binDir"
    } else {
        "bin"
    }
}

/// `git+<repo>#<commit>` / `github:<owner>/<repo>#<commit>` (bun `Repository::fmt`).
pub(crate) fn repository_label(res: &Resolution) -> String {
    match res {
        Resolution::Git { .. } | Resolution::Github { .. } => res.patch_label(),
        _ => String::new(),
    }
}

/// bun `Stringifier::version_to_write`.
fn version_to_write(flat: &Flat<'_>, h: &Hoisted, has_scoped: bool) -> LockfileVersion {
    let loaded = LockfileVersion::from_graph(flat.graph);
    if !has_scoped && loaded < LockfileVersion::V3 {
        return loaded.max(LockfileVersion::V1);
    }
    for rows in &h.rows {
        for &d in rows {
            let p = flat.resolutions[d as usize];
            if p == INVALID {
                continue;
            }
            match flat.res(p) {
                Resolution::Npm {
                    tarball_url,
                    integrity,
                    ..
                } => {
                    if integrity.is_some() {
                        continue;
                    }
                    if let Some(url) = tarball_url
                        && !url_is_under_registry(url, super::DEFAULT_REGISTRY)
                    {
                        return LockfileVersion::V1;
                    }
                }
                Resolution::Git {
                    resolved_commit, ..
                } if !super::is_safe_resolved_tag(resolved_commit) => {
                    return LockfileVersion::V1;
                }
                _ => {}
            }
        }
    }
    if has_scoped {
        LockfileVersion::V3
    } else {
        LockfileVersion::V2
    }
}

/// Overrides in bun's written order: flat rules by name, then scoped rules.
/// `(parent (name, range), target name, target range, value)`.
type ScopedRule<'g> = (Option<(&'g str, &'g str)>, &'g str, &'g str, &'g str);

struct Overrides<'g> {
    /// `(name, value)`, sorted by name.
    flat: Vec<(&'g str, &'g str)>,
    /// `(parent (name, range), target name, target range, value)`, parent-less first.
    scoped: Vec<ScopedRule<'g>>,
}

fn collect_overrides(graph: &LockGraph) -> Overrides<'_> {
    let mut flat: BTreeMap<&str, &str> = BTreeMap::new();
    let mut scoped: Vec<ScopedRule<'_>> = Vec::new();
    for r in &graph.overrides {
        let s = &r.selector;
        if s.parent.is_none() && s.target.range.is_empty() {
            flat.insert(&s.target.name, &r.value);
            continue;
        }
        let parent = s
            .parent
            .as_ref()
            .map(|p| (p.name.as_str(), p.range.as_str()));
        let rule = (
            parent,
            s.target.name.as_str(),
            s.target.range.as_str(),
            r.value.as_str(),
        );
        match scoped
            .iter_mut()
            .find(|e| e.0 == rule.0 && e.1 == rule.1 && e.2 == rule.2)
        {
            Some(e) => *e = rule,
            None => scoped.push(rule),
        }
    }
    scoped.sort_by(|l, r| {
        l.0.is_some()
            .cmp(&r.0.is_some())
            .then_with(|| match (l.0, r.0) {
                (Some(a), Some(b)) => {
                    a.0.as_bytes()
                        .cmp(b.0.as_bytes())
                        .then(a.1.as_bytes().cmp(b.1.as_bytes()))
                }
                _ => std::cmp::Ordering::Equal,
            })
            .then(l.1.as_bytes().cmp(r.1.as_bytes()))
            .then(l.2.as_bytes().cmp(r.2.as_bytes()))
    });
    Overrides {
        flat: flat.into_iter().collect(),
        scoped,
    }
}

fn selector_key(name: &str, range: &str) -> String {
    if range.is_empty() {
        name.to_owned()
    } else {
        format!("{name}@{range}")
    }
}

impl W<'_, '_> {
    fn override_rule(&mut self, indent: u32, key: &str, value: &str) {
        let saved = self.indent;
        self.indent = indent;
        self.write_indent();
        self.indent = saved;
        self.q(key);
        self.s(": ");
        self.q(value);
        self.s(",\n");
    }

    /// bun `write_override_rules`: scoped rules live in selector-keyed objects; a flat rule
    /// folds into its name's unranged group as `"."`.
    fn override_rules(&mut self, o: &Overrides<'_>) {
        let indent = self.indent;
        let solo_end = o
            .scoped
            .iter()
            .position(|r| r.0.is_some())
            .unwrap_or(o.scoped.len());
        let (mut f, mut s, mut g) = (0usize, 0usize, solo_end);
        while f < o.flat.len() || s < solo_end || g < o.scoped.len() {
            let solo = (s < solo_end).then(|| (o.scoped[s].1, o.scoped[s].2));
            let parented = o.scoped.get(g).and_then(|r| r.0);
            let group = match (solo, parented) {
                (Some(a), Some(b)) => Some(
                    if (b.0.as_bytes(), b.1.as_bytes()) < (a.0.as_bytes(), a.1.as_bytes()) {
                        b
                    } else {
                        a
                    },
                ),
                (a, b) => a.or(b),
            };
            if let Some(&(fname, fvalue)) = o.flat.get(f) {
                let flat_first = match group {
                    None => true,
                    Some((gname, glit)) => match fname.as_bytes().cmp(gname.as_bytes()) {
                        std::cmp::Ordering::Less => true,
                        std::cmp::Ordering::Equal => !glit.is_empty(),
                        std::cmp::Ordering::Greater => false,
                    },
                };
                if flat_first {
                    self.override_rule(indent, fname, fvalue);
                    f += 1;
                    continue;
                }
            }
            let Some((gname, glit)) = group else { break };
            self.write_indent();
            self.q(&selector_key(gname, glit));
            self.s(": {\n");
            let dot = if glit.is_empty() {
                match o.flat.get(f) {
                    Some(&(n, v)) if n == gname => {
                        f += 1;
                        Some(v)
                    }
                    _ => None,
                }
            } else {
                match (s < solo_end).then(|| o.scoped[s]) {
                    Some(r) if r.1 == gname && r.2 == glit => {
                        s += 1;
                        Some(r.3)
                    }
                    _ => None,
                }
            };
            if let Some(v) = dot {
                self.override_rule(indent + 1, ".", v);
            }
            while let Some(r) = o.scoped.get(g) {
                if r.0 != Some((gname, glit)) {
                    break;
                }
                self.override_rule(indent + 1, &selector_key(r.1, r.2), r.3);
                g += 1;
            }
            self.write_indent();
            self.s("},\n");
        }
    }

    fn workspace_deps(&mut self, pkg: u32, path: &str, optional_peers: &mut Vec<String>) {
        let flat = self.flat;
        let p: &Package = &flat.graph.packages[pkg as usize];
        let mut any = false;
        if pkg == 0 {
            self.s("\"\": {");
            if !p.name.is_empty() {
                self.s("\n");
                self.inc_indent();
                self.s("\"name\": ");
                self.q(&p.name);
                any = true;
            }
        } else {
            self.q(path);
            self.s(": {\n");
            self.inc_indent();
            self.s("\"name\": ");
            self.q(&p.name);
            if !p.version.is_empty() {
                self.s(",\n");
                self.write_indent();
                self.s("\"version\": \"");
                self.e(&p.version);
                self.s("\"");
            }
            if !p.bin.is_none() {
                self.s(",\n");
                self.write_indent();
                self.s("\"");
                self.s(bin_key(&p.bin));
                self.s("\": ");
                self.bin_multi_line(&p.bin);
            }
            any = true;
        }

        let deps = sorted_by_name(flat, pkg);
        optional_peers.clear();
        for (group_name, group) in GROUPS {
            let mut first = true;
            for &d in &deps {
                let dep = &flat.deps[d as usize];
                if !in_group(dep, group) {
                    continue;
                }
                if dep.behavior.is_optional_peer() {
                    optional_peers.push(dep.name.to_owned());
                }
                if first {
                    if any {
                        self.s(",");
                    }
                    self.s("\n");
                    if any {
                        self.write_indent();
                    } else {
                        self.inc_indent();
                    }
                    self.s("\"");
                    self.s(group_name);
                    self.s("\": {\n");
                    self.inc_indent();
                    any = true;
                    first = false;
                } else {
                    self.s(",\n");
                    self.write_indent();
                }
                self.q(dep.name);
                self.s(": ");
                self.q(dep.literal);
            }
            if !first {
                self.s(",\n");
                self.dec_indent();
                self.s("}");
            }
        }
        let unresolved: Vec<String> = self.unresolved_names(&deps);
        let ops: Vec<String> = std::mem::take(optional_peers);
        for (key, names) in [("optionalPeers", &ops), ("unresolved", &unresolved)] {
            if names.is_empty() {
                continue;
            }
            self.s(",\n");
            self.write_indent();
            self.s("\"");
            self.s(key);
            self.s("\": [\n");
            self.indent += 1;
            for n in names {
                self.write_indent();
                self.q(n);
                self.s(",\n");
            }
            self.dec_indent();
            self.s("]");
        }
        if any {
            self.s(",\n");
            self.dec_indent();
        }
        self.s("},");
    }

    fn bin_multi_line(&mut self, bin: &Bin) {
        match bin {
            Bin::None => {}
            Bin::File { path } | Bin::Dir { path } => self.q(path),
            Bin::NamedFile { name, path } => {
                self.s("{\n");
                self.indent += 1;
                self.write_indent();
                self.q(name);
                self.s(": ");
                self.q(path);
                self.s(",\n");
                self.indent -= 1;
                self.write_indent();
                self.s("}");
            }
            Bin::Map { entries } => {
                self.s("{");
                self.indent += 1;
                if entries.is_empty() {
                    self.s("}");
                    self.indent -= 1;
                    return;
                }
                self.s("\n");
                for (k, v) in entries {
                    self.write_indent();
                    self.q(k);
                    self.s(": ");
                    self.q(v);
                    self.s(",\n");
                }
                self.indent -= 1;
                self.write_indent();
                self.s("}");
            }
        }
    }

    /// The single-line INFO object of a package row.
    fn info(&mut self, row_behavior: Behavior, pkg: u32) {
        let flat = self.flat;
        let p = &flat.graph.packages[pkg as usize];
        let deps = sorted_by_name(flat, pkg);
        self.s("{");
        let mut any = false;
        let mut optional_peers: Vec<String> = Vec::new();
        for (group_name, group) in GROUPS {
            let mut first = true;
            for &d in &deps {
                let dep = &flat.deps[d as usize];
                if !in_group(dep, group) {
                    continue;
                }
                if dep.behavior.is_optional_peer() {
                    optional_peers.push(dep.name.to_owned());
                }
                if first {
                    if any {
                        self.s(",");
                    }
                    self.s(" \"");
                    self.s(group_name);
                    self.s("\": { ");
                    first = false;
                    any = true;
                } else {
                    self.s(", ");
                }
                self.q(dep.name);
                self.s(": ");
                self.q(dep.literal);
            }
            if !first {
                self.s(" }");
            }
        }
        let unresolved = self.unresolved_names(&deps);
        for (key, names) in [
            ("optionalPeers", &optional_peers),
            ("unresolved", &unresolved),
        ] {
            if names.is_empty() {
                continue;
            }
            self.s(", \"");
            self.s(key);
            self.s("\": [");
            for (i, n) in names.iter().enumerate() {
                if i != 0 {
                    self.s(" ");
                }
                self.q(n);
                if i != names.len() - 1 {
                    self.s(",");
                }
            }
            self.s("]");
        }
        let mut key = |w: &mut Self, k: &str| {
            if std::mem::replace(&mut any, true) {
                w.s(",");
            }
            w.s(k);
        };
        if row_behavior.is_bundled() {
            key(self, " \"bundled\": true");
        }
        if p.platform.os != OsSet::ALL {
            key(self, " \"os\": ");
            os_json(&mut self.out, p.platform.os);
        }
        if p.platform.cpu != CpuSet::ALL {
            key(self, " \"cpu\": ");
            cpu_json(&mut self.out, p.platform.cpu);
        }
        if let Some(l) = libc_single(p.platform.libc) {
            key(self, " \"libc\": \"");
            self.s(l);
            self.s("\"");
        }
        if p.meta.needs_scripts() {
            key(self, " \"hasInstallScript\": true");
        }
        if !p.bin.is_none() {
            key(
                self,
                if matches!(p.bin, Bin::Dir { .. }) {
                    " \"binDir\": "
                } else {
                    " \"bin\": "
                },
            );
            bin_single_line(&mut self.out, &p.bin);
        }
        if any {
            self.s(" }");
        } else {
            self.s("}");
        }
    }

    fn integrity_suffix(&mut self, i: Option<&fnpm_types::Integrity>, always: bool) {
        match i {
            Some(i) => {
                self.s(", \"");
                self.s(&i.to_sri());
                self.s("\"]");
            }
            None if always => self.s(", \"\"]"),
            None => self.s("]"),
        }
    }

    fn package_tuple(&mut self, row: &FDep<'_>, pkg: u32) {
        let flat = self.flat;
        let p = &flat.graph.packages[pkg as usize];
        self.s("[\"");
        self.e(&p.name);
        match &p.resolution {
            Resolution::Root => {
                self.s("@root:\", {");
                if p.bin.is_none() {
                    self.s("}]");
                } else {
                    self.s(if matches!(p.bin, Bin::Dir { .. }) {
                        " \"binDir\": "
                    } else {
                        " \"bin\": "
                    });
                    bin_single_line(&mut self.out, &p.bin);
                    self.s(" }]");
                }
            }
            Resolution::Folder { path } | Resolution::Symlink { path } => {
                self.s(if matches!(p.resolution, Resolution::Folder { .. }) {
                    "@file:"
                } else {
                    "@link:"
                });
                self.e(path);
                self.s("\", ");
                self.info(row.behavior, pkg);
                self.s("]");
            }
            Resolution::LocalTarball {
                path: loc,
                integrity,
            }
            | Resolution::RemoteTarball {
                url: loc,
                integrity,
            } => {
                self.s("@");
                self.e(loc);
                self.s("\", ");
                self.info(row.behavior, pkg);
                self.integrity_suffix(integrity.as_ref(), false);
            }
            Resolution::Npm {
                version,
                tarball_url,
                integrity,
            } => {
                self.s("@");
                self.e(version);
                self.s("\", \"");
                if let Some(url) = tarball_url
                    && !url_is_under_registry(url, super::DEFAULT_REGISTRY)
                {
                    self.e(url);
                }
                self.s("\", ");
                self.info(row.behavior, pkg);
                self.integrity_suffix(integrity.as_ref(), true);
            }
            Resolution::Workspace { path } => {
                self.s("@workspace:");
                self.e(path);
                self.s("\"]");
            }
            Resolution::Git {
                resolved_commit,
                integrity,
                ..
            }
            | Resolution::Github {
                resolved_commit,
                integrity,
                ..
            } => {
                self.s("@");
                let label = repository_label(&p.resolution);
                self.e(&label);
                self.s("\", ");
                self.info(row.behavior, pkg);
                self.s(", ");
                self.q(resolved_commit);
                self.integrity_suffix(integrity.as_ref(), false);
            }
        }
    }
}

/// `name@<version>` of a package for `patchedDependencies` keys (bun uses the workspace's
/// version for workspaces and the resolution text otherwise).
fn patch_key(p: &Package) -> String {
    p.patch_key()
}

/// Serialises `graph` (hoisting it first).
pub(crate) fn write(graph: &LockGraph) -> Written {
    let mut flat = Flat::new(graph);
    let hoisted = tree::resolve(&mut flat);
    let text = write_hoisted(&flat, &hoisted);
    Written {
        text,
        cycle: hoisted.cycle,
    }
}

pub(crate) fn write_hoisted(flat: &Flat<'_>, h: &Hoisted) -> String {
    let graph = flat.graph;
    let folders: Vec<Folder> = tree::folders(flat, h);
    let names_len = flat
        .deps
        .iter()
        .map(|d| d.name_id as usize + 1)
        .max()
        .unwrap_or(0);
    let mut placed_names = vec![false; names_len];
    for rows in &h.rows {
        for &r in rows {
            placed_names[flat.deps[r as usize].name_id as usize] = true;
        }
    }
    let any_unresolved = flat
        .deps
        .iter()
        .zip(&flat.resolutions)
        .any(|(d, &r)| r == INVALID && d.behavior.is_optional());
    let mut w = W {
        out: String::with_capacity(256 + graph.packages.len() * 200),
        indent: 0,
        flat,
        placed_names,
        any_unresolved,
    };
    let overrides = collect_overrides(graph);
    let has_scoped = !overrides.scoped.is_empty();

    w.s("{\n");
    w.inc_indent();
    let v = version_to_write(flat, h, has_scoped);
    w.s(&format!("\"lockfileVersion\": {},\n", v as u32));
    w.write_indent();
    w.s(&format!(
        "\"configVersion\": {},\n",
        u8::from(graph.config_version)
    ));
    w.write_indent();
    w.s("\"workspaces\": {\n");
    w.inc_indent();
    let mut optional_peers = Vec::new();
    w.workspace_deps(0, "", &mut optional_peers);
    let mut members: Vec<(&str, u32)> = graph
        .packages
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.resolution {
            Resolution::Workspace { path } => Some((path.as_str(), i as u32)),
            _ => None,
        })
        .collect();
    members.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for (path, id) in members {
        w.s("\n");
        w.write_indent();
        w.workspace_deps(id, path, &mut optional_peers);
    }
    w.s("\n");
    w.dec_indent();
    w.s("},\n");

    // Trusted and patched packages found in the tree, in tree order.
    let mut trusted_found: Vec<(u64, &str)> = Vec::new();
    let mut patched_found: Vec<(u64, String)> = Vec::new();
    let mut tree_ids: Vec<usize> = (0..h.rows.len()).collect();
    tree_ids.retain(|&t| !h.rows[t].is_empty());
    for t in tree_ids {
        for &d in &h.rows[t] {
            let pkg = flat.resolutions[d as usize];
            if pkg == INVALID {
                continue;
            }
            let p = &graph.packages[pkg as usize];
            if !graph.patched_dependencies.is_empty() {
                let key = patch_key(p);
                if graph.patched_dependencies.contains_key(&key) {
                    patched_found.push((string_hash(&key), key));
                }
            }
            if let Some(trusted) = &graph.trusted_dependencies {
                let name = if matches!(p.resolution, Resolution::Npm { .. }) {
                    p.name.as_str()
                } else {
                    flat.deps[d as usize].name
                };
                if trusted.contains(name) {
                    trusted_found.push((string_hash(name), name));
                }
            }
        }
    }
    if !trusted_found.is_empty() {
        let keys: Vec<u64> = trusted_found.iter().map(|t| t.0).collect();
        let reserve = graph.trusted_dependencies.as_ref().map_or(0, |t| t.len());
        let order = written_order(&keys, reserve);
        w.write_indent();
        w.s("\"trustedDependencies\": [\n");
        w.indent += 1;
        for i in order {
            w.write_indent();
            w.s("\"");
            w.s(trusted_found[i].1);
            w.s("\",\n");
        }
        w.dec_indent();
        w.s("],\n");
    } else if graph
        .trusted_dependencies
        .as_ref()
        .is_some_and(|t| t.is_empty())
    {
        w.write_indent();
        w.s("\"trustedDependencies\": [],\n");
    }
    if !patched_found.is_empty() {
        let keys: Vec<u64> = patched_found.iter().map(|t| t.0).collect();
        let order = written_order(&keys, graph.patched_dependencies.len());
        w.write_indent();
        w.s("\"patchedDependencies\": {\n");
        w.indent += 1;
        for i in order {
            let key = &patched_found[i].1;
            w.write_indent();
            w.q(key);
            w.s(": ");
            w.q(&graph.patched_dependencies[key].path);
            w.s(",\n");
        }
        w.dec_indent();
        w.s("},\n");
    }
    if !overrides.flat.is_empty() || has_scoped {
        w.write_indent();
        w.s("\"overrides\": {\n");
        w.indent += 1;
        if has_scoped {
            w.override_rules(&overrides);
        } else {
            let indent = w.indent;
            for &(n, v) in &overrides.flat {
                w.override_rule(indent, n, v);
            }
        }
        w.dec_indent();
        w.s("},\n");
    }
    if !graph.catalogs.default.is_empty() {
        w.write_indent();
        w.s("\"catalog\": {\n");
        w.indent += 1;
        for (k, v) in &graph.catalogs.default {
            w.write_indent();
            w.q(k);
            w.s(": ");
            w.q(v);
            w.s(",\n");
        }
        w.dec_indent();
        w.s("},\n");
    }
    if !graph.catalogs.named.is_empty() {
        w.write_indent();
        w.s("\"catalogs\": {\n");
        w.indent += 1;
        for (name, entries) in &graph.catalogs.named {
            w.write_indent();
            w.q(name);
            w.s(": {\n");
            w.indent += 1;
            for (k, v) in entries {
                w.write_indent();
                w.q(k);
                w.s(": ");
                w.q(v);
                w.s(",\n");
            }
            w.dec_indent();
            w.s("},\n");
        }
        w.dec_indent();
        w.s("},\n");
    }
    let ext_lines = super::extensions::lines(flat, h, &folders);
    if !ext_lines.is_empty() {
        w.write_indent();
        w.s("\"packageExtensions\": {\n");
        w.indent += 1;
        for line in ext_lines {
            w.write_indent();
            w.s(&line);
        }
        w.dec_indent();
        w.s("},\n");
    }

    w.write_indent();
    w.s("\"packages\": {");
    let mut first = true;
    for folder in &folders {
        for &d in &folder.rows {
            let pkg = flat.resolutions[d as usize];
            if pkg == INVALID {
                continue;
            }
            if first {
                first = false;
                w.s("\n");
                w.inc_indent();
            } else {
                w.s(",\n\n");
                w.write_indent();
            }
            let row = &flat.deps[d as usize];
            w.s("\"");
            w.e(&folder.path);
            if folder.depth != 0 {
                w.s("/");
            }
            w.e(row.name);
            w.s("\": ");
            w.package_tuple(row, pkg);
        }
    }
    if !first {
        w.s(",\n");
        w.dec_indent();
    }
    w.s("}\n");
    w.dec_indent();
    w.s("}\n");
    w.out
}
