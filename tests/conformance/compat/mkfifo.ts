/** The `mkfifo` npm package that bun's tests use, on the `mkfifo` command. */
import { spawnSync } from "node:child_process";

export function mkfifo(path: string, mode: number = 0o644): void {
  const result = spawnSync("mkfifo", ["-m", mode.toString(8), path]);
  if (result.status !== 0) throw new Error(`mkfifo ${path} failed: ${result.stderr?.toString() ?? result.error}`);
}

export default { mkfifo };
