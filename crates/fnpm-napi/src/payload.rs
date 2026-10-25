//! Pure conversions between the JSON payloads JS sends and the core's types, and between
//! core outcomes and what the promise settles with. Nothing here touches Node-API, so it is
//! unit-tested with plain `cargo test`.

use std::any::Any;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A rejection, carried from the core's thread to the JS thread where it becomes an `Error`
/// with `code` and `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// One of the codes documented on [`fnpm_core::Error::code`].
    pub code: &'static str,
    /// The `Display` of the underlying error.
    pub message: String,
}

impl Failure {
    /// The code a panic inside the core is reported with. A panic is a bug; it surfaces as
    /// the generic code so the CLI's error handling still applies.
    pub const PANIC_CODE: &'static str = "CORE_ERROR";

    /// Maps a core error to its stable code and message.
    pub fn from_core(error: &fnpm_core::Error) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }

    /// A payload that does not deserialise is a contract violation between the CLI and the
    /// binding; it is reported as a usage error, with the field serde complained about.
    pub fn invalid_payload(what: &str, error: &serde_json::Error) -> Self {
        Self::from_core(&fnpm_core::Error::Usage {
            message: format!("invalid {what}: {error}"),
        })
    }

    /// Turns a caught panic payload into a failure. `panic!` payloads are `&str` or `String`;
    /// anything else gets a generic message.
    pub fn from_panic(payload: &(dyn Any + Send)) -> Self {
        let detail = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_owned());
        Self {
            code: Self::PANIC_CODE,
            message: format!("internal error in the fnpm core: {detail}"),
        }
    }
}

/// Deserialises a JS payload. `what` names it in the error ("install options").
pub fn parse<T: DeserializeOwned>(value: Value, what: &str) -> Result<T, Failure> {
    serde_json::from_value(value).map_err(|error| Failure::invalid_payload(what, &error))
}

/// Serialises a core result for the promise. The core's result types always serialise; a
/// failure here would be a bug in their `Serialize` impls and is reported as `CORE_ERROR`.
pub fn to_json<T: Serialize>(value: &T) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(|error| Failure {
        code: "CORE_ERROR",
        message: format!("could not serialise the core result: {error}"),
    })
}

/// Folds a core result into what the promise settles with.
pub fn settle<T: Serialize>(result: fnpm_core::Result<T>) -> Result<Value, Failure> {
    match result {
        Ok(value) => to_json(&value),
        Err(error) => Err(Failure::from_core(&error)),
    }
}

#[cfg(test)]
mod tests {
    use fnpm_core::{CoreRequest, CoreResult, InstallOptions};
    use serde_json::json;

    use super::*;

    #[test]
    fn install_options_accept_camel_case_and_ignore_unknown_fields() {
        let opts: InstallOptions = parse(
            json!({
                "cwd": "/project",
                "command": "add",
                "packages": ["react@18"],
                "frozenLockfile": true,
                "ignoreScripts": true,
                "saveTextLockfile": true,
                "group": "devDependencies",
                "updateGroups": { "dev": true },
                "someFutureFlag": 42
            }),
            "install options",
        )
        .unwrap();
        assert_eq!(opts.pm.cwd.to_str(), Some("/project"));
        assert_eq!(opts.command, fnpm_core::options::InstallCommand::Add);
        assert_eq!(opts.packages, ["react@18"]);
        assert!(opts.pm.frozen_lockfile);
        assert!(opts.pm.ignore_scripts);
        assert!(opts.pm.save_text_lockfile);
        assert!(opts.update_groups.dev);
        assert!(!opts.update_groups.prod);
    }

    #[test]
    fn empty_object_is_the_default_install() {
        let opts: InstallOptions = parse(json!({}), "install options").unwrap();
        assert_eq!(opts, InstallOptions::default());
    }

    #[test]
    fn wrong_types_are_usage_errors_naming_the_payload() {
        let failure =
            parse::<InstallOptions>(json!({ "packages": "react" }), "install options").unwrap_err();
        assert_eq!(failure.code, "USAGE");
        assert!(
            failure.message.starts_with("invalid install options: "),
            "{}",
            failure.message
        );

        let failure = parse::<CoreRequest>(json!("ls"), "request").unwrap_err();
        assert_eq!(failure.code, "USAGE");
        assert!(
            failure.message.starts_with("invalid request: "),
            "{}",
            failure.message
        );
    }

    #[test]
    fn unknown_enum_values_are_rejected() {
        let failure = parse::<InstallOptions>(json!({ "command": "explode" }), "install options")
            .unwrap_err();
        assert_eq!(failure.code, "USAGE");
        assert!(failure.message.contains("explode"), "{}", failure.message);
    }

    #[test]
    fn core_request_round_trips() {
        let request: CoreRequest = parse(
            json!({
                "command": ["pm", "cache", "rm"],
                "cwd": "/p",
                "positionals": ["a"],
                "passthrough": ["--", "x"],
                "options": { "all": true },
                "json": true
            }),
            "request",
        )
        .unwrap();
        assert_eq!(request.command, ["pm", "cache", "rm"]);
        assert_eq!(request.options.get("all"), Some(&json!(true)));
        assert!(request.json);
    }

    #[test]
    fn core_errors_keep_their_code() {
        let failure = Failure::from_core(&fnpm_core::Error::UnknownCommand(vec!["nope".into()]));
        assert_eq!(failure.code, "USAGE");
        assert_eq!(failure.message, "unknown command [\"nope\"]");
        let failure = Failure::from_core(&fnpm_core::Error::Usage {
            message: "bad flag".into(),
        });
        assert_eq!(
            failure,
            Failure {
                code: "USAGE",
                message: "bad flag".into()
            }
        );
    }

    #[test]
    fn settle_serialises_results_and_maps_errors() {
        let ok = settle(Ok(CoreResult {
            exit_code: 3,
            data: None,
            lines: Some(vec!["hi".into()]),
        }))
        .unwrap();
        assert_eq!(ok, json!({ "exitCode": 3, "lines": ["hi"] }));

        let err = settle::<CoreResult>(Err(fnpm_core::Error::Usage {
            message: "m".into(),
        }))
        .unwrap_err();
        assert_eq!(err.code, "USAGE");
    }

    #[test]
    fn panic_payloads_become_core_errors() {
        let caught = std::panic::catch_unwind(|| panic!("static message")).unwrap_err();
        let failure = Failure::from_panic(caught.as_ref());
        assert_eq!(failure.code, "CORE_ERROR");
        assert_eq!(
            failure.message,
            "internal error in the fnpm core: static message"
        );

        let n = 7;
        let caught = std::panic::catch_unwind(|| panic!("formatted {n}")).unwrap_err();
        assert!(
            Failure::from_panic(caught.as_ref())
                .message
                .ends_with("formatted 7")
        );

        let caught = std::panic::catch_unwind(|| std::panic::panic_any(5_u8)).unwrap_err();
        assert!(
            Failure::from_panic(caught.as_ref())
                .message
                .ends_with("unknown panic payload")
        );
    }
}
