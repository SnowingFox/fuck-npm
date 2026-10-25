//! Malformed input never panics: random selectors, globs, manifests and editor input.

use fnpm_types::DependencyGroup;
use fnpm_workspace::glob::{Dots, Glob};
use fnpm_workspace::{PackageJson, PackageJsonEditor, Selector};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn string(&mut self, alphabet: &[&str], max: usize) -> String {
        let len = (self.next() as usize) % max;
        (0..len)
            .map(|_| alphabet[(self.next() as usize) % alphabet.len()])
            .collect()
    }
}

const GLOB: &[&str] = &[
    "a", "b", "/", ".", "..", "*", "**", "?", "[", "]", "{", "}", ",", "!", "\\", "^", "@", "(",
    ")", "+", "é", "-", "...",
];

const JSON: &[&str] = &[
    "{",
    "}",
    "[",
    "]",
    "\"",
    "\"a\"",
    "\"dependencies\"",
    "\"name\"",
    ":",
    ",",
    " ",
    "\n",
    "\r\n",
    "//",
    "/*",
    "*/",
    "1",
    "true",
    "null",
    "\\",
    "é",
    "\u{feff}",
    "\"x\"",
];

#[test]
fn selectors_and_globs() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..5_000 {
        let pattern = rng.string(GLOB, 16);
        let path = rng.string(GLOB, 16);
        let _ = Selector::parse(&pattern);
        let g = Glob::new(&pattern);
        let _ = g.matches(&path, Dots::Hide);
        let _ = g.matches(&path, Dots::Match);
        let _ = fnpm_workspace::glob::detect_glob_syntax(&pattern);
    }
}

#[test]
fn manifests_and_editor() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..5_000 {
        let text = rng.string(JSON, 24);
        let _ = PackageJson::parse(&text);
        let mut e = PackageJsonEditor::new(text.clone());
        e.set_dependency(DependencyGroup::Dev, "x", "^1");
        e.set_dependency(DependencyGroup::Peer, "a", "2");
        let _ = e.remove_dependency("a");
        e.set_path("a.b", serde_json::json!(1));
        let _ = e.delete_path("name");
        let _ = e.finish();
    }
    // Starting from valid documents, every edit keeps them valid.
    let docs = [
        "{}",
        "{\n  \"name\": \"a\"\n}\n",
        "{\"dependencies\": {\"b\": \"1\", \"a\": \"1\"}}",
        "{\r\n\t\"devDependencies\": {\r\n\t\t\"a\": \"1\",\r\n\t},\r\n}\r\n",
        "{ // c\n  \"dependencies\": {}, /* d */ \"peerDependencies\": {\"a\": \"*\"}\n}",
    ];
    for doc in docs {
        let mut e = PackageJsonEditor::new(doc.to_owned());
        e.set_dependency(DependencyGroup::Prod, "z", "1");
        e.set_dependency(DependencyGroup::Dev, "a", "2");
        e.set_dependency(DependencyGroup::Peer, "a", "3");
        assert!(e.remove_dependency("z"));
        e.set_path("x.y.z", serde_json::json!({"k": [1, 2]}));
        assert!(e.delete_path("x.y"));
        let out = e.finish();
        assert!(PackageJson::parse(&out).is_ok(), "{doc:?} -> {out:?}");
    }
}
