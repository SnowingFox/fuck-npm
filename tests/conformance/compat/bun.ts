/**
 * The subset of the `bun` module (and the `Bun` global) that bun's install tests use, implemented on node:* so the
 * ported tests run under vitest with their bodies unchanged. Only what the tests touch is here; it is not a
 * general Bun polyfill.
 */
import { spawn as nodeSpawn, spawnSync as nodeSpawnSync, type ChildProcess, type StdioOptions } from "node:child_process";
import { createHash, type BinaryToTextEncoding } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import { createRequire } from "node:module";
import net from "node:net";
import { dirname, join, matchesGlob, relative, sep } from "node:path";
import { Readable } from "node:stream";
import tls from "node:tls";
import { fileURLToPath, pathToFileURL } from "node:url";
import { inspect, isDeepStrictEqual, stripVTControlCharacters } from "node:util";
import zlib from "node:zlib";
import semverLib from "semver";
import { parse as parseToml, stringify as stringifyToml } from "smol-toml";
import { bindSync, NodeServer } from "../../registry/src/server.ts";
import * as bunTest from "./bun-test.ts";
import { readTar } from "../../registry/src/tgz.ts";

type Bytes = Uint8Array<ArrayBuffer>;
type Env = Record<string, string | undefined>;

// ---------------------------------------------------------------------------------------------------------------------
// Streams with bun's convenience readers
// ---------------------------------------------------------------------------------------------------------------------

export interface BunReadableStream extends ReadableStream<Bytes> {
  text(): Promise<string>;
  json(): Promise<any>;
  bytes(): Promise<Bytes>;
  arrayBuffer(): Promise<ArrayBuffer>;
  blob(): Promise<Blob>;
}

function withReaders(stream: ReadableStream<Bytes>): BunReadableStream {
  const s = stream as BunReadableStream;
  s.text = () => new Response(stream).text();
  s.json = () => new Response(stream).json();
  s.bytes = async () => new Uint8Array(await new Response(stream).arrayBuffer()) as Bytes;
  s.arrayBuffer = () => new Response(stream).arrayBuffer();
  s.blob = () => new Response(stream).blob();
  return s;
}

/** A web stream over node `Readable`, created on first access so an unread pipe does not get a reader. */
function webStream(readable: Readable | null): BunReadableStream | undefined {
  if (readable === null) return undefined;
  return withReaders(Readable.toWeb(readable) as unknown as ReadableStream<Bytes>);
}

export function readableStreamToText(stream: ReadableStream): Promise<string> {
  return new Response(stream).text();
}

export function readableStreamToJSON(stream: ReadableStream): Promise<any> {
  return new Response(stream).json();
}

export async function readableStreamToArrayBuffer(stream: ReadableStream): Promise<ArrayBuffer> {
  return new Response(stream).arrayBuffer();
}

// ---------------------------------------------------------------------------------------------------------------------
// Bun.file / Bun.write
// ---------------------------------------------------------------------------------------------------------------------

function toBytes(data: string | ArrayBufferView | ArrayBuffer): Bytes {
  if (typeof data === "string") return new TextEncoder().encode(data) as Bytes;
  if (data instanceof ArrayBuffer) return new Uint8Array(data) as Bytes;
  return new Uint8Array(data.buffer as ArrayBuffer, data.byteOffset, data.byteLength) as Bytes;
}

/**
 * `Bun.file(path)`. A Blob subclass whose content is read from disk when it is read, so it can also be the body
 * of a `Response` (undici reads a Blob through `stream()`).
 */
export class BunFile extends Blob {
  readonly name: string;
  readonly #type: string | undefined;

  constructor(path: string | URL, options: { type?: string } = {}) {
    super([]);
    this.name = typeof path === "string" ? path : fileURLToPath(path);
    this.#type = options.type;
  }

  override get size(): number {
    try {
      return fs.statSync(this.name).size;
    } catch {
      return 0;
    }
  }

  override get type(): string {
    if (this.#type !== undefined) return this.#type;
    if (this.name.endsWith(".json")) return "application/json;charset=utf-8";
    if (this.name.endsWith(".js") || this.name.endsWith(".mjs")) return "text/javascript;charset=utf-8";
    if (this.name.endsWith(".txt")) return "text/plain;charset=utf-8";
    if (this.name.endsWith(".tgz") || this.name.endsWith(".gz")) return "application/gzip";
    return "application/octet-stream";
  }

  get lastModified(): number {
    try {
      return fs.statSync(this.name).mtimeMs;
    } catch {
      return 0;
    }
  }

  async exists(): Promise<boolean> {
    try {
      return (await fsp.stat(this.name)).isFile();
    } catch {
      return false;
    }
  }

  override async text(): Promise<string> {
    return fsp.readFile(this.name, "utf8");
  }

  async json(): Promise<any> {
    return JSON.parse(await this.text());
  }

  override async bytes(): Promise<Bytes> {
    const buffer = await fsp.readFile(this.name);
    return new Uint8Array(buffer.buffer as ArrayBuffer, buffer.byteOffset, buffer.byteLength) as Bytes;
  }

  override async arrayBuffer(): Promise<ArrayBuffer> {
    const bytes = await this.bytes();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
  }

  override stream(): ReadableStream<Bytes> {
    return withReaders(Readable.toWeb(fs.createReadStream(this.name)) as unknown as ReadableStream<Bytes>);
  }

  override slice(start?: number, end?: number, contentType?: string): Blob {
    const bytes = fs.readFileSync(this.name);
    return new Blob([new Uint8Array(bytes.subarray(start, end))], { type: contentType });
  }

  async write(data: WriteData): Promise<number> {
    return write(this.name, data);
  }

  async delete(): Promise<void> {
    await fsp.rm(this.name, { force: true });
  }

  unlink(): Promise<void> {
    return this.delete();
  }

  stat(): Promise<fs.Stats> {
    return fsp.stat(this.name);
  }

  writer() {
    fs.mkdirSync(dirname(this.name), { recursive: true });
    const fd = fs.openSync(this.name, "w");
    return {
      write(chunk: string | ArrayBufferView) {
        return typeof chunk === "string" ? fs.writeSync(fd, chunk) : fs.writeSync(fd, toBytes(chunk));
      },
      flush() {
        fs.fsyncSync(fd);
      },
      end() {
        fs.closeSync(fd);
      },
    };
  }

  override toString() {
    return this.name;
  }
}

export function file(path: string | URL | number, options?: { type?: string }): BunFile {
  if (typeof path === "number") throw new Error("Bun.file(fd) is not supported by the fnpm compat layer");
  return new BunFile(path, options);
}

type WriteData = string | ArrayBufferView | ArrayBuffer | Blob | BunFile | Response | Request | object;

/** `Bun.write(destination, data)`: creates the parent directories and returns the number of bytes written. */
export async function write(destination: string | URL | BunFile, data: WriteData): Promise<number> {
  const path =
    destination instanceof BunFile
      ? destination.name
      : typeof destination === "string"
        ? destination
        : fileURLToPath(destination);
  let bytes: Bytes;
  if (typeof data === "string" || ArrayBuffer.isView(data) || data instanceof ArrayBuffer) {
    bytes = toBytes(data as string | ArrayBufferView | ArrayBuffer);
  } else if (data instanceof BunFile) {
    bytes = await data.bytes();
  } else if (data instanceof Blob || data instanceof Response || data instanceof Request) {
    bytes = new Uint8Array(await data.arrayBuffer()) as Bytes;
  } else {
    // Bun.write refuses a plain object; the tests never pass one, but JSON is the useful reading.
    bytes = toBytes(JSON.stringify(data));
  }
  await fsp.mkdir(dirname(path), { recursive: true });
  await fsp.writeFile(path, bytes);
  return bytes.byteLength;
}

// ---------------------------------------------------------------------------------------------------------------------
// Bun.spawn / Bun.spawnSync
// ---------------------------------------------------------------------------------------------------------------------

type StdioValue = "pipe" | "inherit" | "ignore" | null | undefined | number | Blob | Response | ArrayBufferView | string;

export interface SpawnOptions {
  cmd?: string[];
  cwd?: string;
  env?: Env;
  stdin?: StdioValue;
  stdout?: StdioValue;
  stderr?: StdioValue;
  stdio?: StdioValue[];
  uid?: number;
  gid?: number;
  timeout?: number;
  killSignal?: NodeJS.Signals | number;
  windowsHide?: boolean;
  onExit?: (proc: Subprocess, exitCode: number | null, signalCode: string | null, error?: Error) => void;
  [key: string]: unknown;
}

function nodeStdio(value: StdioValue, fallback: "pipe" | "inherit" | "ignore"): "pipe" | "inherit" | "ignore" | number {
  if (value === undefined) return fallback;
  if (value === null || value === "ignore") return "ignore";
  if (value === "inherit") return "inherit";
  if (typeof value === "number") return value;
  // A Blob/Response/bytes value is written to a pipe.
  return "pipe";
}

/**
 * A script path as the first word: `.js`/`.mjs`/`.cjs`/`.ts` runs under the current node, so `fnpmExe()` works
 * whether or not the file is executable.
 */
function resolveCommand(cmd: string[]): string[] {
  const [first, ...rest] = cmd;
  if (first !== undefined && /\.(c|m)?(j|t)s$/.test(first)) return [process.execPath, first, ...rest];
  return cmd;
}

function cleanEnv(env: Env | undefined): NodeJS.ProcessEnv {
  const out: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(env ?? process.env)) if (value !== undefined) out[key] = String(value);
  return out;
}

class FileSink {
  readonly #stream: NodeJS.WritableStream;
  constructor(stream: NodeJS.WritableStream) {
    this.#stream = stream;
  }
  write(chunk: string | ArrayBufferView | ArrayBuffer): number {
    const bytes = toBytes(chunk);
    this.#stream.write(bytes);
    return bytes.byteLength;
  }
  flush() {
    return 0;
  }
  end(chunk?: string | ArrayBufferView) {
    if (chunk !== undefined) this.write(chunk);
    this.#stream.end();
    return 0;
  }
  unref() {}
  ref() {}
}

export class Subprocess {
  readonly child: ChildProcess;
  readonly exited: Promise<number>;
  exitCode: number | null = null;
  signalCode: string | null = null;
  killed = false;
  #stdout: BunReadableStream | undefined | null = null;
  #stderr: BunReadableStream | undefined | null = null;
  #stdin: FileSink | undefined | null = null;
  #spawnError: Error | null = null;
  #errorText: string | null = null;

  constructor(child: ChildProcess, options: SpawnOptions) {
    this.child = child;
    this.exited = new Promise(resolve => {
      child.once("error", error => {
        // ENOENT / EACCES: bun throws from spawn(); a node child reports it here. Report it as exit 127 with the
        // message on stderr so a test fails on its assertion with a readable message.
        this.#spawnError = error;
        this.#errorText = `fnpm-conformance: failed to spawn ${child.spawnfile}: ${error.message}\n`;
        this.exitCode = 127;
        resolve(127);
        options.onExit?.(this, 127, null, error);
      });
      child.once("close", (code, signal) => {
        if (this.#spawnError) return;
        this.exitCode = code ?? (signal ? 128 + (os_signals[signal] ?? 0) : 1);
        this.signalCode = signal;
        resolve(code === null && signal ? 128 + (os_signals[signal] ?? 0) : (code ?? 1));
        options.onExit?.(this, code, signal);
      });
    });
  }

  get pid(): number | undefined {
    return this.child.pid;
  }

  get stdout(): BunReadableStream | undefined {
    if (this.#stdout === null) this.#stdout = webStream(this.child.stdout);
    return this.#stdout;
  }

  get stderr(): BunReadableStream | undefined {
    if (this.#stderr === null) {
      if (this.child.stderr === null) {
        this.#stderr = undefined;
      } else {
        const self = this;
        const source = this.child.stderr;
        // Append the spawn error, if any, after the real stderr.
        const merged = new ReadableStream<Bytes>({
          start(controller) {
            source.on("data", chunk => {
              if (!closed) controller.enqueue(new Uint8Array(chunk) as Bytes);
            });
            let closed = false;
            const finish = () => {
              if (closed) return;
              closed = true;
              if (self.#errorText) controller.enqueue(new TextEncoder().encode(self.#errorText) as Bytes);
              try {
                controller.close();
              } catch {}
            };
            source.once("end", finish);
            source.once("close", finish);
            self.child.once("error", () => setImmediate(finish));
          },
        });
        this.#stderr = withReaders(merged);
      }
    }
    return this.#stderr;
  }

  get stdin(): FileSink | undefined {
    if (this.#stdin === null) this.#stdin = this.child.stdin ? new FileSink(this.child.stdin) : undefined;
    return this.#stdin;
  }

  get readable() {
    return this.stdout;
  }

  kill(signal?: NodeJS.Signals | number) {
    this.killed = true;
    this.child.kill(signal);
  }

  ref() {
    this.child.ref();
  }

  unref() {
    this.child.unref();
  }

  async [Symbol.asyncDispose]() {
    if (this.exitCode === null && this.signalCode === null) {
      this.kill();
      await this.exited;
    }
  }

  [Symbol.dispose]() {
    if (this.exitCode === null && this.signalCode === null) this.kill();
  }
}

const os_signals: Record<string, number> = { SIGHUP: 1, SIGINT: 2, SIGKILL: 9, SIGTERM: 15, SIGABRT: 6 };

function normalizeSpawnArgs(a: string[] | SpawnOptions, b?: SpawnOptions): SpawnOptions {
  return Array.isArray(a) ? { ...b, cmd: a } : a;
}

export function spawn(cmdOrOptions: string[] | SpawnOptions, maybeOptions?: SpawnOptions): Subprocess {
  const options = normalizeSpawnArgs(cmdOrOptions, maybeOptions);
  const cmd = resolveCommand(options.cmd ?? []);
  if (cmd.length === 0) throw new TypeError("spawn: cmd must not be empty");
  const stdio: StdioOptions = options.stdio
    ? (options.stdio.map((value, i) => nodeStdio(value, i === 0 ? "ignore" : "pipe")) as StdioOptions)
    : [
        nodeStdio(options.stdin, "ignore"),
        nodeStdio(options.stdout, "pipe"),
        nodeStdio(options.stderr, "inherit"),
      ];
  const child = nodeSpawn(cmd[0], cmd.slice(1), {
    cwd: options.cwd === undefined ? undefined : String(options.cwd),
    env: cleanEnv(options.env),
    stdio,
    uid: options.uid,
    gid: options.gid,
    timeout: options.timeout,
    killSignal: options.killSignal,
    windowsHide: true,
  });
  const proc = new Subprocess(child, options);
  const stdinValue = options.stdio ? options.stdio[0] : options.stdin;
  if (child.stdin && stdinValue !== "pipe" && stdinValue !== undefined && stdinValue !== null) {
    void (async () => {
      try {
        let bytes: Bytes;
        if (typeof stdinValue === "string") bytes = toBytes(stdinValue);
        else if (ArrayBuffer.isView(stdinValue)) bytes = toBytes(stdinValue);
        else bytes = new Uint8Array(await (stdinValue as Blob | Response).arrayBuffer()) as Bytes;
        child.stdin!.end(bytes);
      } catch {
        child.stdin!.end();
      }
    })();
  }
  child.stdin?.on("error", () => {});
  return proc;
}

export interface SyncSubprocess {
  stdout: Buffer;
  stderr: Buffer;
  exitCode: number;
  success: boolean;
  signalCode: string | null;
  pid: number;
}

export function spawnSync(cmdOrOptions: string[] | SpawnOptions, maybeOptions?: SpawnOptions): SyncSubprocess {
  const options = normalizeSpawnArgs(cmdOrOptions, maybeOptions);
  const cmd = resolveCommand(options.cmd ?? []);
  const stdinValue = options.stdin;
  const input =
    typeof stdinValue === "string" || ArrayBuffer.isView(stdinValue) ? toBytes(stdinValue as string) : undefined;
  const result = nodeSpawnSync(cmd[0], cmd.slice(1), {
    cwd: options.cwd === undefined ? undefined : String(options.cwd),
    env: cleanEnv(options.env),
    input,
    stdio: [
      input !== undefined ? "pipe" : nodeStdio(options.stdin, "ignore"),
      nodeStdio(options.stdout, "pipe"),
      nodeStdio(options.stderr, "pipe"),
    ],
    uid: options.uid,
    gid: options.gid,
    timeout: options.timeout,
    maxBuffer: 1024 * 1024 * 1024,
  });
  const errorText = result.error ? `fnpm-conformance: failed to spawn ${cmd[0]}: ${result.error.message}\n` : "";
  const exitCode = result.error ? 127 : (result.status ?? 128);
  return {
    stdout: result.stdout ?? Buffer.alloc(0),
    stderr: Buffer.concat([result.stderr ?? Buffer.alloc(0), Buffer.from(errorText)]),
    exitCode,
    success: exitCode === 0,
    signalCode: result.signal,
    pid: result.pid ?? 0,
  };
}

// ---------------------------------------------------------------------------------------------------------------------
// Bun.$ (a small subset of the bun shell: the command runs under /bin/sh with every interpolation quoted)
// ---------------------------------------------------------------------------------------------------------------------

export class ShellError extends Error {
  stdout: Buffer;
  stderr: Buffer;
  exitCode: number;
  constructor(output: ShellOutput) {
    super(`Failed with exit code ${output.exitCode}\n${output.stderr.toString()}`);
    this.name = "ShellError";
    this.stdout = output.stdout;
    this.stderr = output.stderr;
    this.exitCode = output.exitCode;
  }
  text() {
    return this.stdout.toString();
  }
}

export interface ShellOutput {
  stdout: Buffer;
  stderr: Buffer;
  exitCode: number;
  text(): string;
  json(): any;
  lines(): string[];
  bytes(): Uint8Array;
}

function shellQuote(value: unknown): string {
  if (value === null || value === undefined) return "''";
  if (Array.isArray(value)) return value.map(shellQuote).join(" ");
  if (typeof value === "object" && value !== null && "raw" in value) return String((value as { raw: unknown }).raw);
  if (value instanceof BunFile) return shellQuote(value.name);
  const text = String(value);
  if (/^[A-Za-z0-9_\-+=.,/:@%^]+$/.test(text)) return text;
  return `'${text.replaceAll("'", `'\\''`)}'`;
}

interface ShellState {
  cwd?: string;
  env?: Env;
  throws: boolean;
  quiet: boolean;
}

const shellDefaults: ShellState = { throws: true, quiet: false };

class ShellPromise implements PromiseLike<ShellOutput> {
  readonly #script: string;
  readonly #state: ShellState;
  #promise: Promise<ShellOutput> | null = null;

  constructor(script: string, state: ShellState) {
    this.#script = script;
    this.#state = { ...state };
  }

  cwd(dir: string) {
    // `tempDir()` returns a String object; node wants a primitive.
    this.#state.cwd = dir === undefined ? dir : String(dir);
    return this;
  }
  env(env: Env) {
    this.#state.env = env;
    return this;
  }
  quiet() {
    this.#state.quiet = true;
    return this;
  }
  nothrow() {
    this.#state.throws = false;
    return this;
  }
  throws(value: boolean) {
    this.#state.throws = value;
    return this;
  }

  #run(): Promise<ShellOutput> {
    this.#promise ??= new Promise((resolve, reject) => {
      const child = nodeSpawn("/bin/sh", ["-c", this.#script], {
        cwd: this.#state.cwd ?? shellDefaults.cwd,
        env: cleanEnv(this.#state.env ?? shellDefaults.env),
        stdio: ["ignore", "pipe", "pipe"],
      });
      const out: Buffer[] = [];
      const err: Buffer[] = [];
      child.stdout.on("data", chunk => out.push(chunk));
      child.stderr.on("data", chunk => err.push(chunk));
      child.on("error", reject);
      child.on("close", code => {
        const stdout = Buffer.concat(out);
        const output: ShellOutput = {
          stdout,
          stderr: Buffer.concat(err),
          exitCode: code ?? 1,
          text: () => stdout.toString(),
          json: () => JSON.parse(stdout.toString()),
          lines: () => stdout.toString().split("\n"),
          bytes: () => new Uint8Array(stdout),
        };
        if (output.exitCode !== 0 && this.#state.throws) reject(new ShellError(output));
        else resolve(output);
      });
    });
    return this.#promise;
  }

  then<A = ShellOutput, B = never>(
    onfulfilled?: ((value: ShellOutput) => A | PromiseLike<A>) | null,
    onrejected?: ((reason: any) => B | PromiseLike<B>) | null,
  ): Promise<A | B> {
    return this.#run().then(onfulfilled, onrejected);
  }
  catch<B = never>(onrejected?: ((reason: any) => B | PromiseLike<B>) | null) {
    return this.#run().catch(onrejected);
  }
  finally(fn?: (() => void) | null) {
    return this.#run().finally(fn);
  }
  async text() {
    return (await this.#run()).text();
  }
  async json() {
    return (await this.#run()).json();
  }
  async lines() {
    return (await this.#run()).lines();
  }
  async bytes() {
    return (await this.#run()).bytes();
  }
  async blob() {
    return new Blob([new Uint8Array((await this.#run()).stdout)]);
  }
}

function shellTag(strings: TemplateStringsArray, ...values: unknown[]): ShellPromise {
  let script = strings[0];
  for (let i = 0; i < values.length; i++) script += shellQuote(values[i]) + strings[i + 1];
  return new ShellPromise(script, shellDefaults);
}

export const $ = Object.assign(shellTag, {
  cwd(dir?: string) {
    shellDefaults.cwd = dir === undefined ? dir : String(dir);
    return $;
  },
  env(env?: Env) {
    shellDefaults.env = env;
    return $;
  },
  nothrow() {
    shellDefaults.throws = false;
    return $;
  },
  throws(value: boolean) {
    shellDefaults.throws = value;
    return $;
  },
  escape: (value: string) => shellQuote(value),
  ShellError,
  Shell: class {},
});

// ---------------------------------------------------------------------------------------------------------------------
// Bun.serve / Bun.listen
// ---------------------------------------------------------------------------------------------------------------------

export interface ServeOptions {
  port?: number | string;
  hostname?: string;
  tls?: { cert?: string | Buffer; key?: string | Buffer; passphrase?: string } | Array<{ cert: string; key: string }>;
  fetch: (request: Request, server: Server) => Response | Promise<Response> | undefined | Promise<undefined>;
  error?: (error: Error) => Response | Promise<Response>;
  idleTimeout?: number;
  development?: boolean;
  [key: string]: unknown;
}

export class Server {
  readonly #node: NodeServer;
  readonly hostname: string;
  readonly protocol: "http" | "https";
  pendingRequests = 0;
  readonly development = false;

  constructor(options: ServeOptions) {
    const tlsOptions = Array.isArray(options.tls) ? options.tls[0] : options.tls;
    const bindHost = options.hostname === undefined || options.hostname === "0.0.0.0" ? "127.0.0.1" : options.hostname;
    this.hostname = options.hostname === undefined || options.hostname === "0.0.0.0" ? "localhost" : options.hostname;
    this.protocol = tlsOptions?.cert ? "https" : "http";
    this.#node = new NodeServer(
      async request => {
        this.pendingRequests++;
        try {
          const response = await options.fetch(request, this);
          if (response instanceof Response) return response;
          return new Response("Not Found", { status: 404 });
        } catch (error) {
          if (options.error) return options.error(error as Error);
          return new Response(String((error as Error)?.stack ?? error), { status: 500 });
        } finally {
          this.pendingRequests--;
        }
      },
      {
        hostname: bindHost,
        port: Number(options.port ?? 0),
        tls: tlsOptions?.cert ? { cert: tlsOptions.cert!, key: tlsOptions.key! } : undefined,
      },
    );
  }

  get port(): number {
    return this.#node.port;
  }

  get url(): URL {
    const host = this.hostname.includes(":") ? `[${this.hostname}]` : this.hostname;
    return new URL(`${this.protocol}://${host}:${this.port}/`);
  }

  get address() {
    return { address: this.hostname, port: this.port, family: "IPv4" };
  }

  get listening(): Promise<void> {
    return this.#node.listening;
  }

  stop(_closeActiveConnections?: boolean) {
    this.#node.close();
    return Promise.resolve();
  }

  reload() {
    throw new Error("Server.reload is not supported by the fnpm compat layer");
  }

  ref() {}
  unref() {
    this.#node.server.unref();
  }

  requestIP() {
    return { address: "127.0.0.1", family: "IPv4", port: 0 };
  }

  upgrade() {
    return false;
  }

  [Symbol.dispose]() {
    void this.stop(true);
  }

  async [Symbol.asyncDispose]() {
    await this.stop(true);
  }
}

export function serve(options: ServeOptions): Server {
  return new Server(options);
}

export interface SocketHandler {
  open?(socket: Socket): void;
  data?(socket: Socket, data: Buffer): void;
  close?(socket: Socket, error?: Error): void;
  error?(socket: Socket, error: Error): void;
  drain?(socket: Socket): void;
  end?(socket: Socket): void;
  handshake?(socket: Socket, success: boolean, error: Error | null): void;
}

export class Socket {
  readonly raw: net.Socket;
  data: unknown;
  constructor(raw: net.Socket) {
    this.raw = raw;
  }
  write(data: string | ArrayBufferView) {
    const bytes = toBytes(data);
    this.raw.write(bytes);
    return bytes.byteLength;
  }
  end(data?: string | ArrayBufferView) {
    if (data !== undefined) this.raw.end(toBytes(data));
    else this.raw.end();
    return 0;
  }
  terminate() {
    this.raw.destroy();
  }
  flush() {}
  ref() {
    this.raw.ref();
  }
  unref() {
    this.raw.unref();
  }
  get remoteAddress() {
    return this.raw.remoteAddress;
  }
  get localPort() {
    return this.raw.localPort;
  }
}

export function listen(options: {
  hostname?: string;
  port?: number;
  socket: SocketHandler;
  tls?: { cert: string; key: string } | boolean;
  data?: unknown;
}) {
  const handler = options.socket;
  const onConnection = (raw: net.Socket) => {
    const socket = new Socket(raw);
    socket.data = options.data;
    raw.on("data", chunk => handler.data?.(socket, chunk));
    raw.on("error", error => handler.error?.(socket, error));
    raw.on("end", () => handler.end?.(socket));
    raw.on("drain", () => handler.drain?.(socket));
    raw.on("close", () => handler.close?.(socket));
    handler.open?.(socket);
  };
  const server =
    options.tls && typeof options.tls === "object"
      ? tls.createServer({ cert: options.tls.cert, key: options.tls.key }, onConnection)
      : net.createServer(onConnection);
  const sockets = new Set<net.Socket>();
  server.on("connection", socket => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });
  const hostname = options.hostname ?? "127.0.0.1";
  const host = hostname === "localhost" || hostname === "0.0.0.0" ? "127.0.0.1" : hostname;
  const handle = bindSync(host, options.port ?? 0);
  if (handle !== null) server.listen(handle);
  else server.listen(options.port ?? 0, host);
  return {
    get port() {
      return (server.address() as net.AddressInfo).port;
    },
    hostname,
    stop(force?: boolean) {
      server.close();
      if (force) for (const socket of sockets) socket.destroy();
    },
    ref() {
      server.ref();
    },
    unref() {
      server.unref();
    },
    [Symbol.dispose]() {
      server.close();
      for (const socket of sockets) socket.destroy();
    },
  };
}

export function connect(): never {
  throw new Error("Bun.connect is not supported by the fnpm compat layer");
}

// ---------------------------------------------------------------------------------------------------------------------
// Bun.Archive (tar writer/reader, optional gzip)
// ---------------------------------------------------------------------------------------------------------------------

type ArchiveInput = Record<string, string | ArrayBufferView | ArrayBuffer | Blob> | ArrayBufferView | ArrayBuffer | Blob;

/** ustar name/prefix split, or null when the name needs a pax header. */
function splitTarName(name: string): { prefix: string; base: string } | null {
  if (Buffer.byteLength(name) <= 100) return { prefix: "", base: name };
  for (let cut = name.lastIndexOf("/"); cut > 0; cut = name.lastIndexOf("/", cut - 1)) {
    const prefix = name.slice(0, cut);
    const base = name.slice(cut + 1);
    if (Buffer.byteLength(base) > 100) return null;
    if (Buffer.byteLength(prefix) <= 155) return { prefix, base };
  }
  return null;
}

function tarHeader(name: string, size: number, type: "0" | "5" | "x", mode: number, mtime: number): Buffer {
  const header = Buffer.alloc(512, 0);
  const split = splitTarName(name) ?? { prefix: "", base: name.slice(0, 100) };
  const { prefix, base } = split;
  header.write(base, 0, 100, "utf8");
  header.write(mode.toString(8).padStart(7, "0") + "\0", 100, 8, "ascii");
  header.write("0000000\0", 108, 8, "ascii");
  header.write("0000000\0", 116, 8, "ascii");
  header.write(size.toString(8).padStart(11, "0") + "\0", 124, 12, "ascii");
  header.write(Math.floor(mtime).toString(8).padStart(11, "0") + "\0", 136, 12, "ascii");
  header.write("        ", 148, 8, "ascii");
  header.write(type, 156, 1, "ascii");
  header.write("ustar\0", 257, 6, "ascii");
  header.write("00", 263, 2, "ascii");
  if (prefix) header.write(prefix, 345, 155, "utf8");
  let sum = 0;
  for (const byte of header) sum += byte;
  header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8, "ascii");
  return header;
}

function paxHeader(name: string, mtime: number): Buffer {
  const record = (key: string, value: string) => {
    const body = ` ${key}=${value}\n`;
    let length = Buffer.byteLength(body);
    length += String(length + String(length).length).length;
    return `${length}${body}`;
  };
  const data = Buffer.from(record("path", name));
  const padded = Buffer.alloc(Math.ceil(data.byteLength / 512) * 512, 0);
  data.copy(padded);
  return Buffer.concat([tarHeader("PaxHeader", data.byteLength, "x", 0o644, mtime), padded]);
}

async function entryBytes(value: string | ArrayBufferView | ArrayBuffer | Blob): Promise<Buffer> {
  if (typeof value === "string") return Buffer.from(value);
  if (value instanceof Blob) return Buffer.from(await value.arrayBuffer());
  return Buffer.from(toBytes(value as ArrayBufferView));
}

export async function createTar(entries: Record<string, string | ArrayBufferView | ArrayBuffer | Blob>): Promise<Buffer> {
  const parts: Buffer[] = [];
  // A fixed time keeps archives reproducible, like bun's.
  const mtime = 0;
  for (const [name, value] of Object.entries(entries)) {
    const isDir = name.endsWith("/");
    const data = isDir ? Buffer.alloc(0) : await entryBytes(value);
    if (splitTarName(name) === null) parts.push(paxHeader(name, mtime));
    parts.push(tarHeader(name, data.byteLength, isDir ? "5" : "0", isDir ? 0o755 : 0o644, mtime));
    if (data.byteLength > 0) {
      parts.push(data);
      const pad = (512 - (data.byteLength % 512)) % 512;
      if (pad) parts.push(Buffer.alloc(pad, 0));
    }
  }
  parts.push(Buffer.alloc(1024, 0));
  return Buffer.concat(parts);
}

export class Archive {
  readonly #input: ArchiveInput;
  readonly #options: { compress?: "gzip"; level?: number };

  constructor(input: ArchiveInput, options: { compress?: "gzip"; level?: number } = {}) {
    this.#input = input;
    this.#options = options;
  }

  async #raw(): Promise<Buffer> {
    const input = this.#input;
    if (input instanceof Blob) return Buffer.from(await input.arrayBuffer());
    if (ArrayBuffer.isView(input) || input instanceof ArrayBuffer) return Buffer.from(toBytes(input as ArrayBufferView));
    return createTar(input as Record<string, string | ArrayBufferView | Blob>);
  }

  async bytes(): Promise<Bytes> {
    let data = await this.#raw();
    if (this.#options.compress === "gzip") data = zlib.gzipSync(data, { level: this.#options.level ?? 6 });
    return new Uint8Array(data.buffer as ArrayBuffer, data.byteOffset, data.byteLength) as Bytes;
  }

  async blob(): Promise<Blob> {
    return new Blob([await this.bytes()]);
  }

  /** Entries of an archive given as bytes (gzipped or not), by path. */
  async files(glob?: string): Promise<Map<string, File>> {
    let data = await this.#raw();
    if (data[0] === 0x1f && data[1] === 0x8b) data = zlib.gunzipSync(data);
    const files = new Map<string, File>();
    for (const entry of readTar(data)) {
      if (glob && !matchesGlob(entry.path, glob)) continue;
      files.set(entry.path, new File([new Uint8Array(entry.data)], entry.path.split("/").at(-1)!));
    }
    return files;
  }

  async extract(dir: string): Promise<number> {
    let data = await this.#raw();
    if (data[0] === 0x1f && data[1] === 0x8b) data = zlib.gunzipSync(data);
    let count = 0;
    for (const entry of readTar(data)) {
      const target = join(dir, entry.path);
      await fsp.mkdir(dirname(target), { recursive: true });
      await fsp.writeFile(target, entry.data, { mode: entry.mode || 0o644 });
      count++;
    }
    return count;
  }

  static async write(path: string, input: ArchiveInput | Archive, options: { compress?: "gzip" } = {}) {
    const archive = input instanceof Archive ? input : new Archive(input, options);
    await write(path, await archive.bytes());
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Parsers, hashing, semver, glob, misc
// ---------------------------------------------------------------------------------------------------------------------

function dropUndefined(value: unknown): unknown {
  if (Array.isArray(value)) return value.filter(item => item !== undefined).map(dropUndefined);
  if (value !== null && typeof value === "object" && !(value instanceof Date)) {
    const out: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value)) if (item !== undefined) out[key] = dropUndefined(item);
    return out;
  }
  return value;
}

export const TOML = {
  parse: (text: string) => parseToml(text),
  stringify: (value: object) => stringifyToml(dropUndefined(value) as Record<string, unknown>),
};

/** JSON with comments and trailing commas, which bun.lock (and fnpm.lock) use. */
export function parseJsonc(text: string): any {
  let out = "";
  let i = 0;
  const n = text.length;
  while (i < n) {
    const ch = text[i];
    if (ch === '"') {
      let j = i + 1;
      while (j < n && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
      out += text.slice(i, j + 1);
      i = j + 1;
    } else if (ch === "/" && text[i + 1] === "/") {
      while (i < n && text[i] !== "\n") i++;
    } else if (ch === "/" && text[i + 1] === "*") {
      i = text.indexOf("*/", i + 2);
      i = i === -1 ? n : i + 2;
    } else if (ch === ",") {
      let j = i + 1;
      while (j < n && /\s/.test(text[j])) j++;
      if (text[j] === "}" || text[j] === "]") i++;
      else {
        out += ch;
        i++;
      }
    } else {
      out += ch;
      i++;
    }
  }
  return JSON.parse(out);
}

export const JSONC = { parse: parseJsonc };
export const JSON5 = { parse: parseJsonc, stringify: (value: unknown) => JSON.stringify(value) };

export class CryptoHasher {
  readonly #hash: ReturnType<typeof createHash>;
  readonly algorithm: string;
  constructor(algorithm: string) {
    this.algorithm = algorithm;
    this.#hash = createHash(algorithm);
  }
  update(data: string | ArrayBufferView | ArrayBuffer | Blob) {
    this.#hash.update(data instanceof ArrayBuffer ? new Uint8Array(data) : (data as string | Uint8Array));
    return this;
  }
  digest(encoding?: BinaryToTextEncoding): any {
    return encoding ? this.#hash.digest(encoding) : this.#hash.digest();
  }
  static hash(algorithm: string, data: string | ArrayBufferView, encoding?: BinaryToTextEncoding): any {
    const hasher = new CryptoHasher(algorithm).update(data);
    return encoding ? hasher.digest(encoding) : hasher.digest();
  }
}

/** `Bun.SHA1` and friends: `new SHA1().update(x).digest()`, `SHA1.hash(x, "hex")`. */
function shaClass(algorithm: string) {
  const Sha = function (this: unknown) {
    return new CryptoHasher(algorithm);
  } as unknown as { new (): CryptoHasher; hash(data: string | ArrayBufferView, encoding?: BinaryToTextEncoding): any };
  Sha.hash = (data, encoding) => CryptoHasher.hash(algorithm, data, encoding);
  return Sha;
}

export const SHA1 = shaClass("sha1");
export const SHA256 = shaClass("sha256");
export const SHA512 = shaClass("sha512");
export const MD5 = shaClass("md5");

/**
 * `Bun.hash`: a 64-bit hash as a bigint. Not wyhash: the tests only compare two hashes of the same data with
 * each other, and the wyhash-specific tests are excluded.
 */
export const hash = Object.assign(
  (data: string | ArrayBufferView | ArrayBuffer, seed = 0): bigint => {
    const h = createHash("sha256");
    h.update(String(seed));
    h.update(data instanceof ArrayBuffer ? new Uint8Array(data) : (data as Uint8Array));
    return h.digest().readBigUInt64LE(0);
  },
  {
    wyhash: (data: string | ArrayBufferView, seed = 0) => hash(data, seed),
    crc32: (data: string | ArrayBufferView) => zlib.crc32(typeof data === "string" ? data : toBytes(data)),
  },
);

export const semver = {
  satisfies: (version: string, range: string) => semverLib.satisfies(version, range, { loose: true }),
  order: (a: string, b: string) => semverLib.compare(a, b, { loose: true }),
};

export class Glob {
  readonly pattern: string;
  constructor(pattern: string) {
    this.pattern = pattern;
  }

  match(path: string): boolean {
    return matchesGlob(path, this.pattern);
  }

  *scanSync(options: string | { cwd?: string; onlyFiles?: boolean; dot?: boolean; absolute?: boolean } = {}) {
    const opts = typeof options === "string" ? { cwd: options } : options;
    const cwd = opts.cwd ?? process.cwd();
    const onlyFiles = opts.onlyFiles ?? true;
    const stack = [cwd];
    while (stack.length > 0) {
      const dir = stack.pop()!;
      let entries: fs.Dirent[];
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const entry of entries) {
        const full = join(dir, entry.name);
        const rel = relative(cwd, full).split(sep).join("/");
        const hidden = rel.split("/").some(part => part.startsWith("."));
        const isDir = entry.isDirectory();
        if (isDir) stack.push(full);
        if (hidden && !opts.dot) continue;
        if (onlyFiles && isDir) continue;
        // path.matchesGlob never matches a dot segment with a wildcard; with `dot`, match it as a plain name.
        const matched =
          matchesGlob(rel, this.pattern) ||
          (opts.dot && hidden && matchesGlob(rel.replace(/(^|\/)\./g, "$1\u0001"), this.pattern));
        if (matched) yield opts.absolute ? full : rel;
      }
    }
  }

  async *scan(options: string | { cwd?: string; onlyFiles?: boolean; dot?: boolean; absolute?: boolean } = {}) {
    yield* this.scanSync(options);
  }
}

export function which(command: string, options: { PATH?: string; cwd?: string } = {}): string | null {
  if (command.includes("/")) return fs.existsSync(command) ? command : null;
  for (const dir of (options.PATH ?? process.env.PATH ?? "").split(":")) {
    if (!dir) continue;
    const candidate = join(dir, command);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      if (fs.statSync(candidate).isFile()) return candidate;
    } catch {}
  }
  return null;
}

export function sleep(ms: number | Date): Promise<void> {
  const delay = ms instanceof Date ? ms.getTime() - Date.now() : ms;
  return new Promise(resolve => setTimeout(resolve, Math.max(0, delay)));
}

const sleepBuffer = new Int32Array(new SharedArrayBuffer(4));
export function sleepSync(ms: number) {
  Atomics.wait(sleepBuffer, 0, 0, ms);
}

/** `Bun.deepMatch(subset, object)`: every property of `subset` is in `object` (recursively). */
export function deepMatch(subset: unknown, object: unknown): boolean {
  if (subset === object) return true;
  if (subset === null || object === null || typeof subset !== "object" || typeof object !== "object") {
    return isDeepStrictEqual(subset, object);
  }
  if (Array.isArray(subset)) {
    return Array.isArray(object) && subset.length === object.length && subset.every((v, i) => deepMatch(v, object[i]));
  }
  return Object.entries(subset).every(([key, value]) => deepMatch(value, (object as Record<string, unknown>)[key]));
}

export function deepEquals(a: unknown, b: unknown, strict = false): boolean {
  if (strict) return isDeepStrictEqual(a, b);
  return isDeepStrictEqual(JSON.parse(JSON.stringify(a ?? null)), JSON.parse(JSON.stringify(b ?? null)));
}

export function resolveSync(specifier: string, from: string): string {
  const base = from.endsWith(".js") || from.endsWith(".ts") ? from : join(from, "noop.js");
  return createRequire(base).resolve(specifier);
}

export function stripANSI(text: string): string {
  return stripVTControlCharacters(text);
}

export function stringWidth(text: string): number {
  let width = 0;
  for (const ch of stripVTControlCharacters(text)) {
    const code = ch.codePointAt(0)!;
    if (code < 0x20 || (code >= 0x300 && code < 0x370)) continue;
    const wide =
      (code >= 0x1100 && code <= 0x115f) ||
      (code >= 0x2e80 && code <= 0xa4cf) ||
      (code >= 0xac00 && code <= 0xd7a3) ||
      (code >= 0xf900 && code <= 0xfaff) ||
      (code >= 0xfe30 && code <= 0xfe4f) ||
      (code >= 0xff00 && code <= 0xff60) ||
      (code >= 0xffe0 && code <= 0xffe6) ||
      (code >= 0x20000 && code <= 0x3fffd) ||
      /\p{Extended_Pictographic}/u.test(ch);
    width += wide ? 2 : 1;
  }
  return width;
}

export const gzipSync = (data: string | ArrayBufferView, options?: zlib.ZlibOptions) =>
  new Uint8Array(zlib.gzipSync(typeof data === "string" ? data : toBytes(data), options));
export const gunzipSync = (data: ArrayBufferView | ArrayBuffer) =>
  new Uint8Array(zlib.gunzipSync(data instanceof ArrayBuffer ? new Uint8Array(data) : toBytes(data)));
export const deflateSync = (data: string | ArrayBufferView) =>
  new Uint8Array(zlib.deflateRawSync(typeof data === "string" ? data : toBytes(data)));
export const inflateSync = (data: ArrayBufferView) => new Uint8Array(zlib.inflateRawSync(toBytes(data)));

/** The fnpm version, which takes the place of `Bun.version` in expected output. */
function fnpmVersion(): string {
  if (process.env.FNPM_VERSION) return process.env.FNPM_VERSION;
  try {
    const here = dirname(fileURLToPath(import.meta.url));
    const pkg = JSON.parse(fs.readFileSync(join(here, "..", "..", "..", "packages", "fuck-npm", "package.json"), "utf8"));
    return String(pkg.version);
  } catch {
    return "0.0.0";
  }
}

export const version = fnpmVersion();
export const revision = "";
export const version_with_sha = version;
export const env = process.env;
export const argv = process.argv;
export const main = process.argv[1] ?? "";
export const stdout = { name: "/dev/stdout", writer: () => process.stdout };
export const stderr = { name: "/dev/stderr", writer: () => process.stderr };
export const nanoseconds = () => Number(process.hrtime.bigint());
export const gc = (_force?: boolean) => {
  (globalThis as { gc?: () => void }).gc?.();
};
export { fileURLToPath, inspect, pathToFileURL };
export const Security = {};

/** `Bun.jest(path)`: the test API of bun:test, for helpers that register tests for a caller. */
export const jest = (_path?: string) => bunTest;

/** The `Bun` global. */
export const Bun = {
  jest,
  $,
  Archive,
  CryptoHasher,
  Glob,
  JSON5,
  JSONC,
  MD5,
  SHA1,
  SHA256,
  SHA512,
  Security,
  TOML,
  argv,
  connect,
  deepEquals,
  deepMatch,
  deflateSync,
  env,
  file,
  fileURLToPath,
  gc,
  gunzipSync,
  gzipSync,
  hash,
  inflateSync,
  inspect,
  listen,
  main,
  nanoseconds,
  pathToFileURL,
  readableStreamToArrayBuffer,
  readableStreamToJSON,
  readableStreamToText,
  resolveSync,
  revision,
  semver,
  serve,
  sleep,
  sleepSync,
  spawn,
  spawnSync,
  stderr,
  stdout,
  stringWidth,
  stripANSI,
  version,
  version_with_sha,
  which,
  write,
};

export default Bun;
