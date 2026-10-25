//! Flag parsing for the native leaves: ports of `rejectUnknownFlags` (`src/cli/plan.ts`) and
//! `parseFlags` (`src/cli/parse-flags.ts`), with the same messages and codes.

use std::collections::HashMap;

use crate::error::CliError;
use crate::plan::{consumes_next, is_flag_token};
use crate::spec::{BinDef, FlagDef, LeafDef};

/// A parsed flag value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Str(String),
    /// A repeatable flag's occurrences.
    List(Vec<String>),
}

/// Parsed flags keyed by `FlagDef::key`, plus the positionals.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub values: HashMap<&'static str, Value>,
    pub positionals: Vec<String>,
}

impl Parsed {
    /// `read.bool`: true only for a literal `true`.
    pub fn bool(&self, key: &str) -> bool {
        matches!(self.values.get(key), Some(Value::Bool(true)))
    }

    /// `read.string`: the value, or the last one of a list.
    pub fn string(&self, key: &str) -> Option<&str> {
        match self.values.get(key)? {
            Value::Str(value) => Some(value),
            Value::List(values) => values.last().map(String::as_str),
            Value::Bool(_) => None,
        }
    }

    /// `read.strings`: every value, in order.
    pub fn strings(&self, key: &str) -> Vec<String> {
        match self.values.get(key) {
            Some(Value::Str(value)) => vec![value.clone()],
            Some(Value::List(values)) => values.clone(),
            _ => Vec::new(),
        }
    }

    /// The raw value (`values[key]`).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }
}

/// `flagName`: `json` from `--json=x`, `h` from `-h`; `None` for non-flag tokens.
fn flag_name(token: &str) -> Option<&str> {
    if !is_flag_token(token) {
        return None;
    }
    let body = token
        .strip_prefix("--")
        .or_else(|| token.strip_prefix('-'))
        .unwrap_or(token);
    let name = body.split('=').next().unwrap_or("");
    if name.is_empty() { None } else { Some(name) }
}

fn spell(name: &str) -> String {
    if name.chars().count() > 1 {
        format!("--{name}")
    } else {
        format!("-{name}")
    }
}

/// Letters of a short cluster up to (and including) the first value flag.
fn cluster_letters(body: &str, leaf: &LeafDef) -> Vec<String> {
    let mut letters = Vec::new();
    for letter in body.chars() {
        letters.push(letter.to_string());
        if (leaf.short)(letter).is_some_and(|spec| spec.takes_value) {
            break;
        }
    }
    letters
}

/// `rejectUnknownFlags`, bun's way: `fnpm install has no --nope flag.`, even when another
/// command has the flag.
pub fn reject_unknown_flags(own: &[String], leaf: &LeafDef, bin: &BinDef) -> Result<(), CliError> {
    let command = format!("{} {}", bin.name, help_command(leaf.name));
    let mut index = 0;
    while index < own.len() {
        let token = own[index].as_str();
        index += 1;
        let Some(name) = flag_name(token) else {
            continue;
        };
        let names = if token.starts_with("--") {
            vec![name.to_owned()]
        } else {
            cluster_letters(name, leaf)
        };
        for candidate in names {
            if (leaf.allowed)(&candidate) {
                continue;
            }
            return Err(CliError::usage(
                "UNKNOWN_FLAG",
                format!("{command} has no {} flag.", spell(&candidate)),
            ));
        }
        if consumes_next(token, leaf.long, leaf.short) {
            index += 1;
        }
    }
    Ok(())
}

/// The command bun names in an unknown-flag error (`ci` is `install`).
pub fn help_command(leaf: &str) -> &str {
    if leaf == "ci" { "install" } else { leaf }
}

fn boolean_word(value: &str) -> Option<bool> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

fn assign(values: &mut HashMap<&'static str, Value>, spec: &'static FlagDef, value: Value) {
    if spec.repeatable
        && let Value::Str(text) = &value
    {
        match values.get_mut(spec.key) {
            Some(Value::List(existing)) => existing.push(text.clone()),
            _ => {
                values.insert(spec.key, Value::List(vec![text.clone()]));
            }
        }
        return;
    }
    values.insert(spec.key, value);
}

/// A token that cannot be a value: a flag, but not a lone `-`.
fn looks_like_flag(next: &str) -> bool {
    next.starts_with('-') && next != "-"
}

/// `parseFlags`: values stay the exact strings typed; arity follows bun's clap.
pub fn parse_flags(tokens: &[String], leaf: &LeafDef) -> Result<Parsed, CliError> {
    let mut values = HashMap::new();
    let mut positionals = Vec::new();
    let mut position = 0;
    while position < tokens.len() {
        let token = tokens[position].as_str();
        position += 1;

        if let Some(body) = token.strip_prefix("--")
            && !body.is_empty()
        {
            let (name, inline) = match body.find('=') {
                Some(at) => (&body[..at], Some(&body[at + 1..])),
                None => (body, None),
            };
            let mut spec = (leaf.long)(name);
            let mut negated = false;
            if spec.is_none()
                && let Some(positive) = name.strip_prefix("no-")
                && let Some(found) = (leaf.long)(positive)
                && !found.takes_value
            {
                spec = Some(found);
                negated = true;
            }
            let Some(spec) = spec else {
                return Err(CliError::usage(
                    "UNKNOWN_FLAG",
                    format!("Unknown flag `--{name}`."),
                ));
            };

            if !spec.takes_value {
                let Some(inline) = inline else {
                    assign(&mut values, spec, Value::Bool(!negated));
                    continue;
                };
                match boolean_word(inline) {
                    Some(parsed) if !negated => assign(&mut values, spec, Value::Bool(parsed)),
                    _ => {
                        return Err(CliError::invalid(format!(
                            "`--{name}` does not take a value (got \"{inline}\")."
                        )));
                    }
                }
                continue;
            }

            if let Some(inline) = inline {
                assign(&mut values, spec, Value::Str(inline.to_owned()));
                continue;
            }
            if spec.optional_value {
                assign(&mut values, spec, Value::Bool(true));
                continue;
            }
            match tokens.get(position) {
                Some(next) if !looks_like_flag(next) => {
                    assign(&mut values, spec, Value::Str(next.clone()));
                    position += 1;
                }
                _ => return Err(CliError::missing(format!("`--{name}` requires a value."))),
            }
            continue;
        }

        if token.starts_with('-') && token.len() > 1 && token != "--" {
            let body = &token[1..];
            for (offset, letter) in body.char_indices() {
                let Some(spec) = (leaf.short)(letter) else {
                    return Err(CliError::usage(
                        "UNKNOWN_FLAG",
                        format!("Unknown flag `-{letter}`."),
                    ));
                };
                if !spec.takes_value {
                    assign(&mut values, spec, Value::Bool(true));
                    continue;
                }
                // A short value flag swallows the rest of its cluster: `-Fapp`, `-F=app`.
                let rest = &body[offset + letter.len_utf8()..];
                let rest = rest.strip_prefix('=').unwrap_or(rest);
                if !rest.is_empty() {
                    assign(&mut values, spec, Value::Str(rest.to_owned()));
                } else if spec.optional_value {
                    assign(&mut values, spec, Value::Bool(true));
                } else {
                    match tokens.get(position) {
                        Some(next) if !looks_like_flag(next) => {
                            assign(&mut values, spec, Value::Str(next.clone()));
                            position += 1;
                        }
                        _ => {
                            return Err(CliError::missing(format!(
                                "`-{letter}` requires a value."
                            )));
                        }
                    }
                }
                break;
            }
            continue;
        }

        positionals.push(token.to_owned());
    }
    Ok(Parsed {
        values,
        positionals,
    })
}
