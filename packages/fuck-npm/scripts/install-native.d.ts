/** Install the current platform archive after checking the embedded SHA-512 and version. */
export function installNative(options?: {
  root?: string;
  abi?: string | null;
  fetchImpl?: typeof fetch;
  env?: NodeJS.ProcessEnv;
}): Promise<string>;
