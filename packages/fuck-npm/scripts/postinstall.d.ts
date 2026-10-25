/** Types for `postinstall.js` (plain JS so it runs without a build on install). */

export type PostinstallOptions = {
  /** The installed `fuck-npm` package directory. */
  root?: string;
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform;
  /** napi-rs platform suffix (`linux-x64-gnu`); null when there is no prebuilt. */
  abi?: string | null;
  /** The npm executable used for the `npm install` fallback. */
  npm?: string;
  fetchImpl?: typeof fetch;
};

export function skipReason(options?: Pick<PostinstallOptions, "env" | "platform" | "root">): string | null;
export function nativeVersion(exe: string): string | null;
export function swapIn(source: string, target: string): void;
export function installWithNpm(
  name: string,
  version: string,
  root: string,
  options?: { npm?: string; env?: NodeJS.ProcessEnv },
): boolean;
export function matchesIntegrity(bytes: Uint8Array, integrity: string): boolean;
export function extractFiles(tgz: Uint8Array, destination: string, wanted: string[]): string[];
export function downloadFromRegistry(
  name: string,
  version: string,
  root: string,
  options?: { env?: NodeJS.ProcessEnv; fetchImpl?: typeof fetch },
): Promise<boolean>;
/** `swapped`, `skipped: <why>` or `kept shim: <why>`. Never rejects. */
export function postinstall(options?: PostinstallOptions): Promise<string>;
