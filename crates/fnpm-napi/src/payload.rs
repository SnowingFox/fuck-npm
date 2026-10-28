//! Conversions between the JSON payloads JS sends and the core's types, and between core
//! outcomes and what the promise settles with. Only [`from_js`] and [`callback`] touch
//! Node-API; everything else is unit-tested with plain `cargo test`.

use std::any::Any;
use std::path::Path;

use napi::bindgen_prelude::{Env, FromNapiValue, JsValue, Unknown, ValueType};
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
            message: error.shown_message(),
        }
    }

    /// A `USAGE` failure with `message`.
    pub fn usage(message: impl Into<String>) -> Self {
        Self::from_core(&fnpm_core::Error::usage(message))
    }

    /// A payload that does not deserialise is a contract violation between the CLI and the
    /// binding; it is reported as a usage error, with the field serde complained about.
    pub fn invalid_payload(what: &str, error: impl std::fmt::Display) -> Self {
        Self::usage(format!("invalid {what}: {error}"))
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

/// Converts a JS payload and deserialises it. Values JSON cannot hold (`undefined` at the
/// top level, functions, symbols, `NaN`, nesting deeper than napi-rs allows) are `USAGE`
/// failures like any other mismatch, not synchronous exceptions.
pub fn from_js<T: DeserializeOwned>(
    env: &Env,
    value: Unknown<'_>,
    what: &str,
) -> Result<T, Failure> {
    // SAFETY: `value` is a live value of this call's `env`.
    let json = unsafe { Value::from_napi_value(env.raw(), value.raw()) }
        .map_err(|error| Failure::invalid_payload(what, &error.reason))?;
    parse(json, what)
}

/// `onEvents` must be a function, `undefined` or `null`.
pub fn callback(value: Option<Unknown<'_>>) -> Result<Option<Unknown<'_>>, Failure> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.get_type() {
        Ok(ValueType::Function) => Ok(Some(value)),
        Ok(ValueType::Undefined | ValueType::Null) => Ok(None),
        Ok(other) => Err(Failure::usage(format!(
            "invalid onEvents: expected a function, got {}",
            type_name(other)
        ))),
        Err(error) => Err(Failure::usage(format!(
            "invalid onEvents: {}",
            error.reason
        ))),
    }
}

/// The `typeof` of a value type, for messages.
fn type_name(value_type: ValueType) -> &'static str {
    match value_type {
        ValueType::Undefined => "undefined",
        ValueType::Null => "null",
        ValueType::Boolean => "boolean",
        ValueType::Number => "number",
        ValueType::String => "string",
        ValueType::Symbol => "symbol",
        ValueType::Object => "object",
        ValueType::Function => "function",
        ValueType::BigInt => "bigint",
        _ => "an unsupported value",
    }
}

/// Deserialises a JSON payload. `what` names it in the error ("install options").
pub fn parse<T: DeserializeOwned>(mut value: Value, what: &str) -> Result<T, Failure> {
    integral_numbers(&mut value);
    serde_json::from_value(value).map_err(|error| Failure::invalid_payload(what, &error))
}

/// JS has only doubles; napi-rs turns integral ones into integers only up to `u32::MAX`, so
/// `minimumReleaseAgeMs: 5184000000` (60 days) would arrive as `5184000000.0` and fail a
/// `u64` field. Every integral number that a double represents exactly becomes an integer.
fn integral_numbers(value: &mut Value) {
    /// 2^53: beyond it a double no longer holds every integer.
    const EXACT: f64 = 9_007_199_254_740_992.0;
    match value {
        Value::Number(number) => {
            if number.is_f64()
                && let Some(float) = number.as_f64()
                && float.fract() == 0.0
                && float.abs() <= EXACT
            {
                // In range by the check above, so the casts are exact.
                *number = if float >= 0.0 {
                    serde_json::Number::from(float as u64)
                } else {
                    serde_json::Number::from(float as i64)
                };
            }
        }
        Value::Array(items) => items.iter_mut().for_each(integral_numbers),
        Value::Object(map) => map.values_mut().for_each(integral_numbers),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

/// The core resolves everything against `cwd`, so it must be absolute: an empty or relative
/// `cwd` would silently mean the Node process's working directory (or a parent of it).
pub fn absolute_cwd(cwd: &Path, what: &str) -> Result<(), Failure> {
    if cwd.is_absolute() {
        return Ok(());
    }
    Err(Failure::usage(format!(
        "invalid {what}: `cwd` must be an absolute path, got {:?}",
        cwd.display().to_string()
    )))
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
    fn integral_doubles_fill_wide_integer_fields() {
        // What napi-rs produces for 60 days in ms, and for nested big integers.
        let opts: InstallOptions = parse(
            json!({
                "cwd": "/p",
                "minimumReleaseAgeMs": 5_184_000_000.0,
                "networkConcurrency": 64,
                "depth": 0.0
            }),
            "install options",
        )
        .unwrap();
        assert_eq!(opts.pm.minimum_release_age_ms, Some(5_184_000_000));
        assert_eq!(opts.pm.network_concurrency, Some(64));
        assert_eq!(opts.depth, Some(0));

        let request: CoreRequest = parse(
            json!({
                "cwd": "/p",
                "options": { "big": 9_007_199_254_740_992.0_f64, "neg": -5e9, "frac": 1.5 }
            }),
            "request",
        )
        .unwrap();
        assert_eq!(request.options["big"], json!(9_007_199_254_740_992_u64));
        assert_eq!(request.options["neg"], json!(-5_000_000_000_i64));
        assert_eq!(request.options["frac"], json!(1.5));
    }

    #[test]
    fn fractions_and_out_of_range_numbers_stay_usage_errors() {
        for payload in [
            json!({ "networkConcurrency": 1.5 }),
            json!({ "networkConcurrency": 70_000.0 }),
            // Too large to be exact: left as a double, which a u64 field rejects.
            json!({ "minimumReleaseAgeMs": 1e300 }),
        ] {
            let failure = parse::<InstallOptions>(payload, "install options").unwrap_err();
            assert_eq!(failure.code, "USAGE", "{}", failure.message);
        }
    }

    #[test]
    fn cwd_must_be_absolute() {
        let root = if cfg!(windows) {
            r"C:\project"
        } else {
            "/project"
        };
        assert_eq!(absolute_cwd(Path::new(root), "request"), Ok(()));
        for cwd in ["", ".", "relative/dir"] {
            let failure = absolute_cwd(Path::new(cwd), "install options").unwrap_err();
            assert_eq!(failure.code, "USAGE");
            assert!(
                failure
                    .message
                    .starts_with("invalid install options: `cwd` must be an absolute path"),
                "{}",
                failure.message
            );
        }
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
