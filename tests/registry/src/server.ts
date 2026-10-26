import http from "node:http";
import https from "node:https";
import net, { type AddressInfo, type Socket } from "node:net";
import { Readable } from "node:stream";

export type FetchHandler = (request: Request) => Response | Promise<Response>;

/**
 * A failure or delay that the registry applies to matching requests, before it answers them. Retry tests use it.
 *
 * - `status`: answer with this status (500, 503, 429, ...) instead of the real answer.
 * - `reset`: destroy the socket without an answer (ECONNRESET on the client).
 * - `truncate`: send the headers and half of the real body, then destroy the socket.
 * - `delayMs`: wait before the answer (or before the failure).
 * - `stall`: never answer; the socket stays open until the client gives up or the registry stops.
 * - `times`: how many matching requests fail; the default is every one. A fault with `times` used up does nothing.
 */
export interface Fault {
  /** Matches the path (with the query). A string must equal the path, or the path with `%2f` decoded. */
  path?: string | RegExp | ((path: string, method: string) => boolean);
  method?: string;
  status?: number;
  /** Sent with a 429 or 503. */
  retryAfter?: string | number;
  body?: string;
  reset?: boolean;
  truncate?: boolean;
  stall?: boolean;
  delayMs?: number;
  times?: number;
}

interface ActiveFault extends Fault {
  hits: number;
}

export interface ServeOptions {
  hostname?: string;
  port?: number;
  tls?: { key: string | Buffer; cert: string | Buffer };
  /** Delay every answer by this many milliseconds. */
  latencyMs?: number;
  /** Called for each request a fault answered (or dropped); status 0 for reset/stall/truncate. */
  onFault?: (request: { method: string; path: string; headers: Record<string, string> }, status: number) => void;
}

function matches(fault: ActiveFault, path: string, method: string): boolean {
  if (fault.times !== undefined && fault.hits >= fault.times) return false;
  if (fault.method !== undefined && fault.method.toUpperCase() !== method) return false;
  const pattern = fault.path;
  if (pattern === undefined) return true;
  if (typeof pattern === "function") return pattern(path, method);
  if (pattern instanceof RegExp) return pattern.test(path);
  return pattern === path || pattern === path.replace(/%2f/gi, "/");
}

const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

interface TcpHandle {
  bind(address: string, port: number): number;
  bind6(address: string, port: number, flags: number): number;
  close(): void;
}

/**
 * A TCP handle bound synchronously. `server.listen(port, host)` resolves the host first, so the port is only known
 * a tick later; bun's tests read `registry.port` right after `start()`. Returns null when the address is not
 * numeric or the internal binding is unavailable, and the caller falls back to `listen(port, host)`.
 */
export function bindSync(host: string, port: number): TcpHandle | null {
  const family = net.isIP(host);
  if (family === 0) return null;
  try {
    const { TCP, constants } = (process as unknown as { binding(name: string): any }).binding("tcp_wrap");
    const handle: TcpHandle = new TCP(constants.SERVER);
    const error = family === 4 ? handle.bind(host, port) : handle.bind6(host, port, 0);
    if (error !== 0) {
      handle.close();
      return null;
    }
    return handle;
  } catch {
    return null;
  }
}

/**
 * Serves a fetch-style handler on node:http, the way `Bun.serve` does for bun. For a numeric host the port is
 * bound before the constructor returns, so `port` is known synchronously. On the default host (127.0.0.1) the same
 * port is also opened on ::1 when it is free, so `http://localhost:<port>` works whichever address a client tries.
 */
export class NodeServer {
  readonly server: http.Server;
  /** The ::1 twin of a 127.0.0.1 server. */
  readonly #twin: http.Server | null = null;
  readonly faults: ActiveFault[] = [];
  readonly #sockets = new Set<Socket>();
  readonly #handler: FetchHandler;
  readonly #options: ServeOptions;
  readonly listening: Promise<void>;
  #closed = false;

  constructor(handler: FetchHandler, options: ServeOptions = {}) {
    this.#handler = handler;
    this.#options = options;
    const listener = (req: http.IncomingMessage, res: http.ServerResponse) => void this.#serve(req, res);
    this.server = options.tls
      ? https.createServer({ key: options.tls.key, cert: options.tls.cert }, listener)
      : http.createServer(listener);
    this.server.keepAliveTimeout = 60_000;
    this.server.headersTimeout = 0;
    this.server.requestTimeout = 0;
    this.server.on("connection", socket => {
      socket.setNoDelay(true);
      this.#sockets.add(socket);
      socket.on("close", () => this.#sockets.delete(socket));
    });
    this.server.on("secureConnection", socket => {
      this.#sockets.add(socket);
      socket.on("close", () => this.#sockets.delete(socket));
    });
    const hostname = options.hostname ?? "127.0.0.1";
    this.listening = new Promise((resolve, reject) => {
      this.server.once("listening", () => resolve());
      this.server.once("error", reject);
    });
    const handle = bindSync(hostname, options.port ?? 0);
    if (handle !== null) this.server.listen(handle);
    else this.server.listen(options.port ?? 0, hostname);

    if (hostname === "127.0.0.1" && handle !== null) {
      const twinHandle = bindSync("::1", this.port);
      if (twinHandle !== null) {
        const twin = options.tls
          ? https.createServer({ key: options.tls.key, cert: options.tls.cert }, listener)
          : http.createServer(listener);
        twin.keepAliveTimeout = 60_000;
        twin.headersTimeout = 0;
        twin.requestTimeout = 0;
        twin.on("connection", socket => {
          this.#sockets.add(socket);
          socket.on("close", () => this.#sockets.delete(socket));
        });
        twin.on("error", () => {});
        twin.listen(twinHandle);
        this.#twin = twin;
      }
    }
  }

  get port(): number {
    const address = this.server.address() as AddressInfo | null;
    if (address === null) throw new Error("The server is not listening");
    return address.port;
  }

  addFault(fault: Fault): Fault {
    const active: ActiveFault = { ...fault, hits: 0 };
    this.faults.push(active);
    return active;
  }

  clearFaults() {
    this.faults.length = 0;
  }

  async #serve(req: http.IncomingMessage, res: http.ServerResponse) {
    const method = (req.method ?? "GET").toUpperCase();
    const path = req.url ?? "/";
    try {
      if (this.#options.latencyMs) await sleep(this.#options.latencyMs);
      const fault = this.faults.find(candidate => matches(candidate, path, method));
      if (fault !== undefined) {
        fault.hits++;
        if (fault.delayMs) await sleep(fault.delayMs);
        if (fault.stall || fault.reset || fault.status !== undefined || fault.truncate) {
          const headers: Record<string, string> = {};
          for (const [key, value] of Object.entries(req.headers)) if (typeof value === "string") headers[key] = value;
          this.#options.onFault?.({ method, path, headers }, fault.status ?? 0);
        }
        if (fault.stall) return;
        if (fault.reset) {
          req.socket.destroy();
          return;
        }
        if (fault.status !== undefined) {
          const headers: Record<string, string> = { "content-type": "application/json" };
          if (fault.retryAfter !== undefined) headers["retry-after"] = String(fault.retryAfter);
          res.writeHead(fault.status, headers);
          res.end(fault.body ?? JSON.stringify({ error: `injected ${fault.status}` }));
          return;
        }
      }

      const host = req.headers.host ?? `127.0.0.1:${this.port}`;
      const protocol = this.#options.tls ? "https" : "http";
      const headers = new Headers();
      for (let i = 0; i < req.rawHeaders.length; i += 2) headers.append(req.rawHeaders[i], req.rawHeaders[i + 1]);
      const hasBody = method !== "GET" && method !== "HEAD";
      // A request sent through a proxy carries the absolute URL (`GET http://host/path`); Bun.serve
      // hands that URL to the handler as it is.
      const absolute = /^https?:\/\//i.test(path);
      const request = new Request(absolute ? path : `${protocol}://${host}${path}`, {
        method,
        headers,
        body: hasBody ? (Readable.toWeb(req) as ReadableStream) : undefined,
        // @ts-expect-error: required by undici for a streamed body
        duplex: hasBody ? "half" : undefined,
      });
      const response = await this.#handler(request);
      const body = response.body === null ? null : Buffer.from(await response.arrayBuffer());
      const outHeaders: Record<string, string | string[]> = {};
      response.headers.forEach((value, key) => {
        if (key === "set-cookie") return;
        outHeaders[key] = value;
      });
      const cookies = response.headers.getSetCookie();
      if (cookies.length > 0) outHeaders["set-cookie"] = cookies;
      if (body !== null && outHeaders["content-length"] === undefined) {
        outHeaders["content-length"] = String(body.byteLength);
      }
      res.writeHead(response.status, outHeaders);
      if (fault?.truncate && body !== null) {
        res.write(body.subarray(0, Math.floor(body.byteLength / 2)), () => req.socket.destroy());
        return;
      }
      res.end(method === "HEAD" ? undefined : (body ?? undefined));
    } catch (error) {
      if (!res.headersSent) {
        res.writeHead(500, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: String(error) }));
      } else {
        res.destroy();
      }
    }
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.server.close();
    this.server.closeAllConnections();
    this.#twin?.close();
    this.#twin?.closeAllConnections();
    for (const socket of this.#sockets) socket.destroy();
  }
}
