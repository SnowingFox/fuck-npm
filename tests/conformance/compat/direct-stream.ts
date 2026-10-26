/**
 * bun's `new ReadableStream({ type: "direct", pull(controller) })`, which bun's tests use to send a response body in
 * pieces: the controller has `write(chunk)`, `flush()` and `close()`, and `pull` runs once for the whole body. Node
 * only knows byte and default streams, so the conformance setup installs this subclass as the global.
 */
type DirectController = {
  write(chunk: string | ArrayBufferView | ArrayBuffer): number;
  flush(): Promise<void>;
  close(): void;
  end(): void;
};
type DirectSource = { type: "direct"; pull(controller: DirectController): unknown; cancel?(reason?: unknown): unknown };
const NodeReadableStream = globalThis.ReadableStream;
class BunReadableStream<R = unknown> extends NodeReadableStream<R> {
  constructor(source?: UnderlyingSource<R> | DirectSource, strategy?: QueuingStrategy<R>) {
    if (source?.type !== "direct") {
      super(source as UnderlyingSource<R>, strategy);
      return;
    }
    const direct = source as DirectSource;
    const encoder = new TextEncoder();
    let started = false;
    super({
      async pull(inner) {
        if (started) return;
        started = true;
        let closed = false;
        const controller: DirectController = {
          write(chunk) {
            const bytes =
              typeof chunk === "string"
                ? encoder.encode(chunk)
                : chunk instanceof ArrayBuffer
                  ? new Uint8Array(chunk)
                  : new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
            // A copy: the caller may reuse its buffer after write() returns.
            inner.enqueue(bytes.slice() as R);
            return bytes.byteLength;
          },
          // Each write is its own chunk already; let the reader take it before the next write.
          flush: () => new Promise(resolve => setTimeout(resolve, 0)),
          close() {
            if (!closed) inner.close();
            closed = true;
          },
          end() {
            controller.close();
          },
        };
        await direct.pull(controller);
        controller.close();
      },
      cancel: reason => direct.cancel?.(reason),
    });
  }
}

/** Makes `globalThis.ReadableStream` accept `type: "direct"`. */
export function installDirectStreams() {
  globalThis.ReadableStream = BunReadableStream as typeof ReadableStream;
}

export { BunReadableStream };
