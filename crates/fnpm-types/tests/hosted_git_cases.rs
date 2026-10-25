//! The hosted-git-info test vectors bun uses (`test/cli/install/hosted-git-info/cases.ts`,
//! converted to JSON in `fixtures/hosted_git_info_cases.json`).

use fnpm_types::{HostedGitInfo, Representation};
use serde_json::Value;

fn cases() -> Value {
    serde_json::from_str(include_str!("fixtures/hosted_git_info_cases.json")).expect("fixture parses")
}

fn representation_name(r: Representation) -> &'static str {
    match r {
        Representation::Shortcut => "shortcut",
        Representation::Sshurl => "sshurl",
        Representation::Https => "https",
        Representation::Git => "git",
        Representation::Http => "http",
    }
}

#[test]
fn valid_urls() {
    let cases = cases();
    let valid = cases["valid"].as_object().unwrap();
    let mut failures = Vec::new();
    for (url, expected) in valid {
        let info = match HostedGitInfo::from_url(url) {
            Ok(Some(info)) => info,
            other => {
                failures.push(format!("{url}: got {other:?}"));
                continue;
            }
        };
        let mut check = |field: &str, got: Option<&str>| {
            if let Some(want) = expected.get(field).and_then(Value::as_str)
                && Some(want) != got
            {
                failures.push(format!("{url}: {field} expected {want:?}, got {got:?}"));
            }
        };
        check("type", Some(info.provider.type_str()));
        check("user", info.user.as_deref());
        check("project", Some(&info.project));
        check("committish", info.committish.as_deref());
        check("default", Some(representation_name(info.default_representation)));
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), valid.len(), failures.join("\n"));
}
