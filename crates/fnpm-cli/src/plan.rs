//! Dispatch from argv alone: which invocations run natively, which print the version, and
//! which are handed to the TS CLI. A port of the parts of `packages/fuck-npm/src/cli/plan.ts`
//! and `foreign-flags.ts` that apply to the native leaves; every other path only has to be
//! recognised as "not native".

use crate::spec::{
    BinDef, FlagDef, IGNORED_FOREIGN_PREFIXES, IGNORED_RUNTIME_SHORT, LeafDef, RootChild,
    ignored_foreign_flag, ignored_foreign_option, ignored_runtime_flag,
};

/// Where an invocation goes.
#[derive(Debug)]
pub enum Route {
    /// Exec the TS CLI with the original argv.
    Node,
    /// Print the version and exit 0.
    Version,
    /// Run a native command.
    Native(Invocation),
}

/// A native command after the command path is resolved and foreign flags are translated.
#[derive(Debug)]
pub struct Invocation {
    pub leaf: &'static LeafDef,
    /// Our tokens: flags and positionals after the command path.
    pub own: Vec<String>,
    /// Tokens after a user-typed `--`.
    pub forwarded: Vec<String>,
    /// `--json` among our tokens.
    pub wants_json: bool,
}

/// `token.startsWith("-") && token !== "-" && token !== "--"`.
pub(crate) fn is_flag_token(token: &str) -> bool {
    token.starts_with('-') && token != "-" && token != "--"
}

/// `consumesNext`: whether `token` is a value flag that takes the next token as its value.
pub(crate) fn consumes_next(
    token: &str,
    long: fn(&str) -> Option<&'static FlagDef>,
    short: fn(char) -> Option<&'static FlagDef>,
) -> bool {
    if token.contains('=') {
        return false;
    }
    let spec = if let Some(name) = token.strip_prefix("--") {
        long(name)
    } else {
        let mut chars = token.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some(_), Some(letter), None) => short(letter),
            _ => None,
        }
    };
    spec.is_some_and(|spec| spec.takes_value && !spec.optional_value)
}

/// Resolve `args` (argv after the program name) for `bin`.
///
/// The root is walked like `planInvocation`: flags (and the values of value flags) are
/// stepped over, so `fnpm --cwd app install` finds `install`. The first command token
/// decides: a native leaf is planned here, anything else goes to Node. With no command,
/// `-v`/`--version` anywhere prints the version (the TS CLI's "group + --version" rule);
/// everything else (help, `--tree`, unknown commands) goes to Node.
pub fn route(bin: &'static BinDef, args: &[String]) -> Route {
    if args.is_empty() {
        return Route::Node;
    }
    let dash = args.iter().position(|arg| arg == "--");
    let (head, tail) = match dash {
        Some(index) => (&args[..index], &args[index + 1..]),
        None => (args, &[][..]),
    };

    let mut rest: Vec<String> = Vec::new();
    let mut leaf: Option<&'static LeafDef> = None;
    let mut index = 0;
    while index < head.len() {
        let token = head[index].as_str();
        if is_flag_token(token) {
            rest.push(token.to_owned());
            if consumes_next(token, bin.root_long, bin.root_short) && index + 1 < head.len() {
                rest.push(head[index + 1].clone());
                index += 1;
            }
            index += 1;
            continue;
        }
        if (bin.root_alias)(token) {
            return Route::Node;
        }
        match (bin.root_child)(token) {
            None => break,
            Some(RootChild::Other) => return Route::Node,
            Some(RootChild::Native(found)) => {
                leaf = Some(found);
                index += 1;
                break;
            }
        }
    }
    rest.extend(head[index..].iter().cloned());

    let has = |tokens: &[String], short: &str, long: &str| {
        tokens.iter().any(|token| token == short || token == long)
    };

    let Some(leaf) = leaf else {
        return if has(&rest, "-v", "--version") {
            Route::Version
        } else {
            Route::Node
        };
    };

    let tokens = strip_runtime_flags(translate_foreign_flags(rest, leaf), leaf);
    if has(&tokens, "-t", "--tree") || has(&tokens, "-h", "--help") {
        return Route::Node;
    }
    if has(&tokens, "-v", "--version") {
        return Route::Version;
    }
    let wants_json = tokens.iter().any(|token| token == "--json");
    Route::Native(Invocation {
        leaf,
        own: tokens,
        forwarded: tail.to_vec(),
        wants_json,
    })
}

/// clap's `IgnoredLongFlags::value_is_next_arg` (`ignoredValueIsNext`): `None` when `name`
/// is not skipped, otherwise whether the following token is its value.
fn ignored_value_is_next(name: &str, next: Option<&str>, leaf: &LeafDef) -> Option<bool> {
    if ignored_foreign_option(name) {
        return Some(next.is_some_and(|next| !next.starts_with('-')));
    }
    let positive = name.strip_prefix("no-");
    if ignored_foreign_flag(name)
        || positive.is_some_and(|positive| {
            ignored_foreign_flag(positive)
                || ignored_foreign_option(positive)
                || (leaf.own)(positive)
        })
    {
        return Some(matches!(next, Some("true" | "false")));
    }
    if IGNORED_FOREIGN_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Some(false);
    }
    None
}

/// `translateForeignFlags`: rewrite npm/yarn/pnpm flags to ours and drop the ignored ones
/// (with their values). Positionals and the command's own flags pass through.
pub fn translate_foreign_flags(tokens: Vec<String>, leaf: &LeafDef) -> Vec<String> {
    if leaf.foreign.is_none() {
        return tokens;
    }
    let mut out = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        if !token.starts_with("--") || token == "--" {
            out.push(token.to_owned());
            continue;
        }
        let next = tokens.get(index).map(String::as_str);
        let equals = token.find('=');
        let name = match equals {
            Some(at) => &token[2..at],
            None => &token[2..],
        };

        if let Some(direct) = (leaf.rewrite)(token) {
            if !direct.is_empty() {
                out.push(direct.to_owned());
            }
            continue;
        }
        if equals.is_none()
            && let Some(next) = next
            && let Some(joined) = (leaf.rewrite)(&format!("{token}={next}"))
        {
            if !joined.is_empty() {
                out.push(joined.to_owned());
            }
            index += 1;
            continue;
        }

        if (leaf.own)(name) {
            out.push(token.to_owned());
            continue;
        }
        if equals.is_none()
            && name
                .strip_prefix("no-")
                .is_some_and(|positive| (leaf.own_boolean)(positive))
        {
            out.push(token.to_owned());
            continue;
        }

        if let Some(value_is_next) = ignored_value_is_next(name, next, leaf) {
            if value_is_next && equals.is_none() {
                index += 1;
            }
            continue;
        }
        out.push(token.to_owned());
    }
    out
}

/// `stripRuntimeFlags`: drop bun's runtime-only switches unless the leaf declares one.
pub fn strip_runtime_flags(tokens: Vec<String>, leaf: &LeafDef) -> Vec<String> {
    if !leaf.ignores_runtime_flags {
        return tokens;
    }
    tokens
        .into_iter()
        .filter(|token| {
            if token == IGNORED_RUNTIME_SHORT {
                return false;
            }
            let Some(name) = token.strip_prefix("--") else {
                return true;
            };
            (leaf.own)(name) || !ignored_runtime_flag(name)
        })
        .collect()
}
