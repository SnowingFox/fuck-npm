//! Rewrites `npm run` / `yarn <cmd>` / `pnpm run` / `npx` / `pnpm dlx` / `pnpx` inside scripts
//! to `fnpm run` / `fnpm x`, so scripts re-enter fnpm instead of spawning another package
//! manager (bun `lifecycle_script_runner.rs` `replace_package_manager_run`, bug for bug).

/// The executable name written into rewritten scripts.
pub const BIN_NAME: &str = "fnpm";

/// Yarn built-in subcommands (union of v1 + v2.3 sets). Sorted for `binary_search`.
const YARN_COMMANDS: &[&str] = &[
    "access",
    "add",
    "audit",
    "autoclean",
    "bin",
    "cache",
    "check",
    "config",
    "create",
    "dedupe",
    "dlx",
    "exec",
    "explain",
    "generate-lock-entry",
    "generateLockEntry",
    "global",
    "help",
    "import",
    "info",
    "init",
    "install",
    "licenses",
    "link",
    "list",
    "login",
    "logout",
    "node",
    "npm",
    "outdated",
    "owner",
    "pack",
    "patch",
    "plugin",
    "policies",
    "publish",
    "rebuild",
    "remove",
    "run",
    "set",
    "tag",
    "team",
    "unlink",
    "unplug",
    "up",
    "upgrade",
    "upgrade-interactive",
    "upgradeInteractive",
    "version",
    "versions",
    "why",
    "workspace",
    "workspaces",
];

/// Returns `script` with package-manager invocations replaced. A word only matches at the start
/// of the script or after a space or quote.
pub fn replace_package_manager_run(script: &str) -> String {
    let script = script.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(script.len() + 8);
    let run = |out: &mut Vec<u8>| {
        out.extend_from_slice(BIN_NAME.as_bytes());
        out.extend_from_slice(b" run");
    };
    let x = |out: &mut Vec<u8>| {
        out.extend_from_slice(BIN_NAME.as_bytes());
        out.extend_from_slice(b" x ");
    };

    let mut i = 0;
    let mut delimiter: u8 = b' ';
    while i < script.len() {
        let rest = &script[i..];
        match script[i] {
            b'y' => {
                if delimiter > 0
                    && let Some(next) = rest.strip_prefix(b"yarn ")
                    && let Some(space) = next.iter().position(|&c| c == b' ')
                {
                    let cmd = &next[..space];
                    if cmd == b"run" {
                        run(&mut out);
                        i += b"yarn run".len();
                        continue;
                    }
                    // `yarn npm` is a yarn 2 subcommand.
                    if cmd == b"npm" {
                        i += b"yarn npm ".len();
                        out.extend_from_slice(b"yarn npm ");
                        continue;
                    }
                    if cmd.first() == Some(&b'-') {
                        i += b"yarn ".len() + cmd.len();
                        out.extend_from_slice(b"yarn ");
                        out.extend_from_slice(cmd);
                        continue;
                    }
                    // Implicit `yarn <script>`.
                    let known = std::str::from_utf8(cmd)
                        .is_ok_and(|c| YARN_COMMANDS.binary_search(&c).is_ok());
                    if !known {
                        run(&mut out);
                        out.push(b' ');
                        out.extend_from_slice(cmd);
                        i += b"yarn ".len() + cmd.len();
                        delimiter = 0;
                        continue;
                    }
                }
                delimiter = 0;
            }
            b' ' => delimiter = b' ',
            b'"' => delimiter = b'"',
            b'\'' => delimiter = b'\'',
            b'n' => {
                if delimiter > 0 {
                    if rest.starts_with(b"npm run ") {
                        run(&mut out);
                        i += b"npm run ".len();
                        let rewritten = append_npm_run_in_workspaces(&mut out, &script[i..]);
                        if rewritten == 0 {
                            out.push(b' ');
                        }
                        i += rewritten;
                        delimiter = 0;
                        continue;
                    }
                    if rest.starts_with(b"npx ") {
                        x(&mut out);
                        i += b"npx ".len();
                        delimiter = 0;
                        continue;
                    }
                }
                delimiter = 0;
            }
            b'p' => {
                if delimiter > 0 {
                    if rest.starts_with(b"pnpm run ") {
                        run(&mut out);
                        out.push(b' ');
                        i += b"pnpm run ".len();
                        delimiter = 0;
                        continue;
                    }
                    if rest.starts_with(b"pnpm dlx ") {
                        x(&mut out);
                        i += b"pnpm dlx ".len();
                        delimiter = 0;
                        continue;
                    }
                    if rest.starts_with(b"pnpx ") {
                        x(&mut out);
                        i += b"pnpx ".len();
                        delimiter = 0;
                        continue;
                    }
                }
                delimiter = 0;
            }
            _ => delimiter = 0,
        }
        out.push(script[i]);
        i += 1;
    }
    // Only whole ASCII words were replaced, so `out` is valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// npm takes `-w` / `--workspaces` anywhere, `fnpm run` only in front. Returns the bytes of
/// `args` consumed (0: nothing rewritten).
fn append_npm_run_in_workspaces(out: &mut Vec<u8>, args: &[u8]) -> usize {
    let end = args
        .iter()
        .position(|c| b";&|\r\n".contains(c))
        .unwrap_or(args.len());
    let command = args[..end].trim_ascii_end();
    if command.iter().any(|c| b"'\"()$`\\#".contains(c)) {
        return 0;
    }

    let mut flags: Vec<u8> = Vec::new();
    let mut rest: Vec<u8> = Vec::new();
    let mut selects_workspaces = false;
    let mut tokens = command
        .split(|c| *c == b' ' || *c == b'\t')
        .filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        let workspace = match token {
            b"-w" | b"--workspace" => tokens.next(),
            b"--workspaces" | b"--ws" => {
                selects_workspaces = true;
                flags.extend_from_slice(b" --workspaces");
                continue;
            }
            b"--if-present" => {
                flags.extend_from_slice(b" --if-present");
                continue;
            }
            b"--" => {
                for passed_on in std::iter::once(token).chain(tokens.by_ref()) {
                    rest.push(b' ');
                    rest.extend_from_slice(passed_on);
                }
                break;
            }
            _ => match token
                .strip_prefix(b"--workspace=")
                .or_else(|| token.strip_prefix(b"-w="))
            {
                Some(w) => Some(w),
                None => {
                    rest.push(b' ');
                    rest.extend_from_slice(token);
                    continue;
                }
            },
        };
        // It must not be the next flag, or a word with a redirect attached to it.
        let Some(workspace) = workspace.filter(|w| {
            !w.is_empty() && w[0] != b'-' && !w.iter().any(|c| *c == b'<' || *c == b'>')
        }) else {
            return 0;
        };
        selects_workspaces = true;
        flags.extend_from_slice(b" --filter=");
        // npm also accepts the workspace's folder. `--filter` tells a path from a name by the
        // leading dot.
        if workspace.contains(&b'/') && !matches!(workspace[0], b'@' | b'.' | b'/') {
            flags.extend_from_slice(b"./");
        }
        flags.extend_from_slice(workspace);
    }

    if !selects_workspaces {
        return 0;
    }
    out.extend_from_slice(&flags);
    out.extend_from_slice(&rest);
    command.len()
}

#[cfg(test)]
mod tests {
    use super::replace_package_manager_run as r;

    #[test]
    fn yarn_commands_sorted() {
        let mut sorted = super::YARN_COMMANDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, super::YARN_COMMANDS);
    }

    #[test]
    fn npm_and_npx() {
        assert_eq!(r("npm run build"), "fnpm run build");
        assert_eq!(
            r("npm run build && npm run test"),
            "fnpm run build && fnpm run test"
        );
        assert_eq!(r("npx tsc --noEmit"), "fnpm x tsc --noEmit");
        assert_eq!(r("echo npm run x"), "echo fnpm run x");
        // Not at a word start.
        assert_eq!(r("xnpm run build"), "xnpm run build");
        assert_eq!(r("echo \"npm run a\""), "echo \"fnpm run a\"");
        assert_eq!(r("echo 'npx a'"), "echo 'fnpm x a'");
        // `npm run` without a following space is left alone.
        assert_eq!(r("npm run"), "npm run");
        assert_eq!(r("npm install"), "npm install");
    }

    #[test]
    fn pnpm() {
        assert_eq!(r("pnpm run build"), "fnpm run build");
        assert_eq!(r("pnpm dlx create-vite"), "fnpm x create-vite");
        assert_eq!(r("pnpx cowsay hi"), "fnpm x cowsay hi");
        assert_eq!(r("pnpm install"), "pnpm install");
    }

    #[test]
    fn yarn() {
        assert_eq!(r("yarn run build"), "fnpm run build");
        assert_eq!(r("yarn build && echo"), "fnpm run build && echo");
        assert_eq!(r("yarn add left-pad"), "yarn add left-pad");
        assert_eq!(r("yarn npm publish"), "yarn npm publish");
        assert_eq!(r("yarn --cwd x build"), "yarn --cwd x build");
        // The command needs a space after it.
        assert_eq!(r("yarn build"), "yarn build");
        assert_eq!(r("yarnx build x"), "yarnx build x");
    }

    #[test]
    fn npm_workspaces() {
        assert_eq!(r("npm run build -w pkg"), "fnpm run --filter=pkg build");
        assert_eq!(
            r("npm run build --workspace=packages/a"),
            "fnpm run --filter=./packages/a build"
        );
        assert_eq!(
            r("npm run build --workspace @s/a"),
            "fnpm run --filter=@s/a build"
        );
        assert_eq!(
            r("npm run test --workspaces --if-present"),
            "fnpm run --workspaces --if-present test"
        );
        assert_eq!(
            r("npm run build -w a && echo"),
            "fnpm run --filter=a build && echo"
        );
        assert_eq!(
            r("npm run build -w a -- --x"),
            "fnpm run --filter=a build -- --x"
        );
        // Shell syntax or a missing value: left as it is.
        assert_eq!(r("npm run build -w \"a\""), "fnpm run build -w \"a\"");
        assert_eq!(r("npm run build -w"), "fnpm run build -w");
        assert_eq!(r("npm run build -w --x"), "fnpm run build -w --x");
        assert_eq!(r("npm run build -w a>log"), "fnpm run build -w a>log");
    }

    #[test]
    fn utf8_preserved() {
        assert_eq!(r("echo héllo && npm run b"), "echo héllo && fnpm run b");
    }
}
