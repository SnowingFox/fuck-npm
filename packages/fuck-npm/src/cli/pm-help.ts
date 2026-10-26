import type { Colors } from "../output/output.ts";

/**
 * `fnpm pm`, `fnpm pm --help` and `fnpm pm <unknown>`: bun's package manager help, word for
 * word (bun `PackageManagerCommand::print_help`), with bun's colour tags.
 */
const PM_HELP = `
<b>Usage<r>: <b><green>fnpm pm<r> <cyan>[flags]<r> <blue>[<command>]<r>

  Run package manager utilities.

<b>Commands:<r>

  <b><green>fnpm pm<r> <blue>scan<r>                 scan all packages in lockfile for security vulnerabilities
  <b><green>fnpm pm<r> <blue>pack<r>                 create a tarball of the current workspace
  <d>├<r> <cyan>--dry-run<r>                 do everything except for writing the tarball to disk
  <d>├<r> <cyan>--destination<r>             the directory the tarball will be saved in
  <d>├<r> <cyan>--filename<r>                the name of the tarball
  <d>├<r> <cyan>--ignore-scripts<r>          don't run pre/postpack and prepare scripts
  <d>├<r> <cyan>--gzip-level<r>              specify a custom compression level for gzip (0-9, default is 9)
  <d>└<r> <cyan>--quiet<r>                   only output the tarball filename
  <b><green>fnpm pm<r> <blue>bin<r>                  print the path to bin folder
  <d>└<r> <cyan>-g<r>                        print the <b>global<r> path to bin folder
  <b><green>fnpm pm<r> <blue>ls<r>                   list the tree of installed dependencies
  <d>├<r> <cyan>names ...<r>                 list every installed package with one of these names
  <d>├<r> <cyan>--all<r>                     list the entire tree of installed dependencies
  <d>├<r> <cyan>--trusted<r>                 list only trusted dependencies
  <d>├<r> <cyan>--prod<r>                    omit devDependencies
  <d>└<r> <cyan>-g<r>                        list the tree of <b>globally<r> installed dependencies
  <b><green>fnpm pm<r> <blue>why<r> <d><pkg><r>            show dependency tree explaining why a package is installed
  <b><green>fnpm pm<r> <blue>diff<r> <d>[a] [b]<r>         show what changed between two versions of a package (or vs a folder/tarball)
  <d>├<r> <d>fnpm pm diff react<r>            installed version → latest
  <d>├<r> <d>fnpm pm diff react@18.2.0 19.0.0<r>
  <d>├<r> <d>fnpm pm diff axios@1.6.0:lib 1.6.1<r>  only files under lib/ <d>(also<r> <d>:file.js<r><d>, or paths after the two sides)<r>
  <d>├<r> <cyan>--stat<r>, <cyan>--name-only<r>       summarize instead of printing hunks
  <d>├<r> <cyan>-U<r> <d>n<r>                      lines of context (default 3)
  <d>└<r> <cyan>--json<r>                    one JSON document (files, patch text, notes, totals)
  <b><green>fnpm pm<r> <blue>licenses<r>             list installed packages grouped by license
  <d>├<r> <cyan>--json<r>                    output as JSON
  <d>├<r> <cyan>--prod<r>                    omit devDependencies
  <d>├<r> <cyan>--dev<r>                     list only what devDependencies pull in
  <d>├<r> <cyan>--long<r>                    also print author, description and homepage
  <d>└<r> <cyan>--filter<r> <d><pattern><r>        list only the matching workspaces' dependencies
  <b><green>fnpm pm<r> <blue>whoami<r>               print the current npm username
  <b><green>fnpm pm<r> <blue>view<r> <d>name[@version] [property ...]<r>  view package metadata from the registry <d>(use \`fnpm info\` instead)<r>
  <b><green>fnpm pm<r> <blue>version<r> <d>[increment]<r>  bump the version in package.json and create a git tag
  <d>└<r> <cyan>increment<r>                 patch, minor, major, prepatch, preminor, premajor, prerelease, from-git, or a specific version
  <b><green>fnpm pm<r> <blue>pkg<r>                  manage data in package.json
  <d>├<r> <cyan>get<r> <d>[key ...]<r>
  <d>├<r> <cyan>set<r> <d>key=value ...<r>
  <d>├<r> <cyan>delete<r> <d>key ...<r>
  <d>└<r> <cyan>fix<r>                       auto-correct common package.json errors
  <b><green>fnpm pm<r> <blue>hash<r>                 generate & print the hash of the current lockfile
  <b><green>fnpm pm<r> <blue>hash-string<r>          print the string used to hash the lockfile
  <b><green>fnpm pm<r> <blue>hash-print<r>           print the hash stored in the current lockfile
  <b><green>fnpm pm<r> <blue>cache<r>                print the path to the cache folder
  <b><green>fnpm pm<r> <blue>cache rm<r>             clear the cache
  <d>└<r> <cyan>--dry-run<r>                 print what would be deleted without deleting anything
  <b><green>fnpm pm<r> <blue>cache prune<r>          remove the packages that are older than 30 days
  <d>├<r> <cyan>--max-age<r> <d><days><r>          age threshold in days (default 30)
  <d>└<r> <cyan>--dry-run<r>                 print what would be removed without deleting anything
  <b><green>fnpm pm<r> <blue>cache pack<r><d>|<r><blue>unpack<r> <d>file<r>  save / restore the cache entries this lockfile needs as one file (for CI caches)
  <b><green>fnpm pm<r> <blue>fetch<r>                download all dependencies into the cache without installing
  <b><green>fnpm pm<r> <blue>migrate<r>              migrate another package manager's lockfile without installing anything
  <b><green>fnpm pm<r> <blue>untrusted<r>            print current untrusted dependencies with scripts
  <b><green>fnpm pm<r> <blue>trust<r> <d>names ...<r>      run scripts for untrusted dependencies and add to \`trustedDependencies\`
  <d>├<r>  <cyan>--all<r>                    trust all untrusted dependencies
  <d>└<r>  <cyan>--dry-run<r>                print the scripts that would run, without running them or saving
  <b><green>fnpm pm<r> <blue>default-trusted<r>      print the default trusted dependencies list
`;

/** The help text, coloured like bun's `<b>`, `<d>` and colour tags, or plain. */
export function renderPmHelp(colors: Colors): string {
  const styles: Record<string, (text: string) => string> = {
    b: colors.bold,
    d: colors.dim,
    green: colors.green,
    blue: colors.blue,
    cyan: colors.cyan,
    magenta: colors.magenta,
  };
  // Every tag runs to the next `<r>`; nested tags (`<b><green>`) stack.
  return PM_HELP.replace(/((?:<(?:b|d|green|blue|cyan|magenta)>)+)(.*?)<r>/g, (_, tags: string, text: string) =>
    tags
      .slice(1, -1)
      .split("><")
      .reduceRight((inner, tag) => styles[tag]!(inner), text),
  );
}
