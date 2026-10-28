// BridgeClient: a reconnecting WebSocket client for the NFC Browser Bridge (protocol v1).

import {
  DEFAULT_URL,
  PROTOCOL_VERSION,
  type ApduResponse,
  type CardPresentEvent,
  type CardRemovedEvent,
  type ErrorBody,
  type ErrorCode,
  type HelloEvent,
  type Reader,
  type ReaderStatus,
  type RecoveryEvent,
  type RecoveryState,
  type Request,
  type RequestId,
  type ResultMap,
  type ServiceStatus,
  type UsbReader,
} from "./protocol.js";

export type ConnectionState = "connecting" | "open" | "closed";

/** Client-side failures in addition to the bridge's error codes. */
export type ClientErrorCode = ErrorCode | "timeout" | "disconnected";

export class BridgeError extends Error {
  readonly code: ClientErrorCode;
  readonly pcsc?: string;
  readonly completed?: number;

  constructor(code: ClientErrorCode, message: string, extra: { pcsc?: string; completed?: number } = {}) {
    super(message);
    this.name = "BridgeError";
    this.code = code;
    this.pcsc = extra.pcsc;
    this.completed = extra.completed;
  }

  static from(body: ErrorBody): BridgeError {
    return new BridgeError(body.code, body.message, body);
  }
}

/** The subset of the browser WebSocket the client uses (so tests can inject a fake). */
export interface WebSocketLike {
  readonly readyState: number;
  onopen: ((ev: unknown) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  send(data: string): void;
  close(code?: number, reason?: string): void;
}

export type WebSocketFactory = new (url: string) => WebSocketLike;

export interface BridgeClientOptions {
  url?: string;
  WebSocket?: WebSocketFactory;
  /** Base timeout per request. Transmit adds `perApduTimeoutMs` per APDU. */
  requestTimeoutMs?: number;
  perApduTimeoutMs?: number;
  minBackoffMs?: number;
  maxBackoffMs?: number;
}

export type ReadersChange = ReaderStatus;

export interface BridgeClientEvents {
  state: ConnectionState;
  hello: HelloEvent;
  /** Full reader list; fired on hello and on every change. */
  readers: ReadersChange;
  /** A step the bridge took to bring a stuck reader back (macOS). */
  recovery: RecoveryEvent;
  /** A new tap. Cards already present at connect time are in `readers`, not here. */
  card_present: CardPresentEvent;
  card_removed: CardRemovedEvent;
  /** Fatal problems, e.g. a bridge that speaks another protocol version (no reconnect). */
  error: BridgeError;
}

type Handler<T> = (value: T) => void;

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: BridgeError) => void;
  timer: ReturnType<typeof setTimeout>;
}

const OPEN = 1;

export class BridgeClient {
  readonly url: string;
  state: ConnectionState = "closed";
  service: ServiceStatus = "unavailable";
  readers: Reader[] = [];
  /** Smart-card readers on USB; null where the bridge cannot see USB, or when not connected. */
  usbReaders: UsbReader[] | null = null;
  /** Reader recovery; null where the bridge cannot see USB, or when not connected. */
  recovery: RecoveryState | null = null;
  /** Bridge app version from `hello`. */
  version: string | null = null;

  private readonly opts: Required<Omit<BridgeClientOptions, "url" | "WebSocket">>;
  private readonly WS: WebSocketFactory;
  private ws: WebSocketLike | null = null;
  private wanted = false;
  private backoff: number;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private nextId = 1;
  private readonly pending = new Map<RequestId, Pending>();
  private readonly handlers = new Map<keyof BridgeClientEvents, Set<Handler<never>>>();

  constructor(options: BridgeClientOptions = {}) {
    this.url = options.url ?? DEFAULT_URL;
    const WS = options.WebSocket ?? (globalThis as { WebSocket?: WebSocketFactory }).WebSocket;
    if (!WS) throw new Error("No WebSocket implementation available");
    this.WS = WS;
    this.opts = {
      requestTimeoutMs: options.requestTimeoutMs ?? 5000,
      perApduTimeoutMs: options.perApduTimeoutMs ?? 25,
      minBackoffMs: options.minBackoffMs ?? 500,
      maxBackoffMs: options.maxBackoffMs ?? 10_000,
    };
    this.backoff = this.opts.minBackoffMs;
  }

  on<K extends keyof BridgeClientEvents>(event: K, handler: Handler<BridgeClientEvents[K]>): () => void {
    let set = this.handlers.get(event);
    if (!set) this.handlers.set(event, (set = new Set()));
    set.add(handler as Handler<never>);
    return () => set.delete(handler as Handler<never>);
  }

  private emit<K extends keyof BridgeClientEvents>(event: K, value: BridgeClientEvents[K]): void {
    for (const h of this.handlers.get(event) ?? []) {
      try {
        (h as Handler<BridgeClientEvents[K]>)(value);
      } catch (e) {
        console.error(`BridgeClient ${event} handler threw`, e);
      }
    }
  }

  /** Start connecting; reconnects automatically until `close()`. */
  connect(): void {
    this.wanted = true;
    if (!this.ws && !this.retryTimer) this.open();
  }

  close(): void {
    this.wanted = false;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = null;
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onopen = ws.onclose = ws.onerror = ws.onmessage = null;
      ws.close(1000);
    }
    this.dropped();
    this.setState("closed");
  }

  private setState(state: ConnectionState): void {
    if (this.state !== state) {
      this.state = state;
      this.emit("state", state);
    }
  }

  private open(): void {
    this.setState("connecting");
    let ws: WebSocketLike;
    try {
      ws = new this.WS(this.url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    ws.onmessage = (ev) => this.onMessage(ev.data);
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.dropped();
      this.scheduleReconnect();
    };
    // onclose always follows onerror.
    ws.onerror = () => {};
  }

  /** Connection gone: fail pending requests and forget the reader state. */
  private dropped(): void {
    for (const [id, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(new BridgeError("disconnected", "Connection to the bridge was lost"));
      this.pending.delete(id);
    }
    if (this.readers.length || this.service !== "unavailable" || this.usbReaders !== null) {
      this.readers = [];
      this.service = "unavailable";
      this.usbReaders = null;
      this.recovery = null;
      this.emit("readers", { service: this.service, readers: [] });
    }
  }

  private scheduleReconnect(): void {
    if (!this.wanted) {
      this.setState("closed");
      return;
    }
    this.setState("connecting");
    const delay = this.backoff;
    this.backoff = Math.min(this.backoff * 2, this.opts.maxBackoffMs);
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      if (this.wanted) this.open();
    }, delay);
  }

  private onMessage(data: unknown): void {
    if (typeof data !== "string") return;
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    if ("event" in msg) this.onEvent(msg);
    else if ("ok" in msg) this.onResponse(msg);
  }

  private onEvent(msg: Record<string, unknown>): void {
    switch (msg.event) {
      case "hello": {
        const hello = msg as unknown as HelloEvent;
        if (hello.protocol !== PROTOCOL_VERSION) {
          this.emit(
            "error",
            new BridgeError("protocol_mismatch", `Bridge speaks protocol ${hello.protocol}, this client ${PROTOCOL_VERSION}`),
          );
          this.close();
          return;
        }
        this.backoff = this.opts.minBackoffMs;
        this.version = hello.version;
        this.setStatus(hello);
        this.setState("open");
        this.emit("hello", hello);
        this.emit("readers", this.status());
        break;
      }
      case "readers": {
        this.setStatus(msg as unknown as ReadersChange);
        this.emit("readers", this.status());
        break;
      }
      case "recovery": {
        this.emit("recovery", msg as unknown as RecoveryEvent);
        break;
      }
      // The bridge sends a `readers` event right after these; update the list first anyway so
      // handlers see a consistent `readers`.
      case "card_present": {
        const ev = msg as unknown as CardPresentEvent;
        this.setCard(ev.reader, { uid: ev.uid, atr: ev.atr });
        this.emit("card_present", ev);
        break;
      }
      case "card_removed": {
        const ev = msg as unknown as CardRemovedEvent;
        this.setCard(ev.reader, null);
        this.emit("card_removed", ev);
        break;
      }
    }
  }

  private setStatus(s: ReaderStatus): void {
    this.service = s.service;
    this.readers = s.readers;
    this.usbReaders = s.usbReaders ?? null;
    this.recovery = s.recovery ?? null;
  }

  private status(): ReadersChange {
    const s: ReadersChange = { service: this.service, readers: this.readers };
    if (this.usbReaders) s.usbReaders = this.usbReaders;
    if (this.recovery) s.recovery = this.recovery;
    return s;
  }

  private setCard(reader: string, card: Reader["card"]): void {
    this.readers = this.readers.map((r) => (r.name === reader ? { ...r, card } : r));
  }

  private onResponse(msg: Record<string, unknown>): void {
    const id = msg.id as RequestId;
    const p = this.pending.get(id);
    if (!p) return;
    this.pending.delete(id);
    clearTimeout(p.timer);
    if (msg.ok === true) p.resolve(msg.result);
    else p.reject(BridgeError.from(msg.error as ErrorBody));
  }

  /** Send a raw request. Rejects with `BridgeError`. */
  request<R extends Request>(req: R, timeoutMs?: number): Promise<ResultMap[R["type"]]> {
    return new Promise((resolve, reject) => {
      const ws = this.ws;
      if (!ws || this.state !== "open" || ws.readyState !== OPEN) {
        reject(new BridgeError("disconnected", "Not connected to the bridge"));
        return;
      }
      const id = this.nextId++;
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new BridgeError("timeout", `Bridge did not answer ${req.type} in time`));
      }, timeoutMs ?? this.opts.requestTimeoutMs);
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });
      ws.send(JSON.stringify({ id, ...req }));
    });
  }

  ping(): Promise<ResultMap["ping"]> {
    return this.request({ type: "ping" });
  }

  listReaders(): Promise<ResultMap["list_readers"]> {
    return this.request({ type: "list_readers" });
  }

  /** Send a batch of hex APDUs in one PC/SC transaction. */
  async transmit(reader: string, apdus: string[], options: { stopOnError?: boolean; unsafe?: boolean } = {}): Promise<ApduResponse[]> {
    const timeout = this.opts.requestTimeoutMs + this.opts.perApduTimeoutMs * apdus.length;
    const result = await this.request({ type: "transmit", reader, apdus, ...options }, timeout);
    return result.responses;
  }

  /** Reconnect to the card with a reset; returns the new ATR. */
  async reset(reader: string): Promise<string> {
    return (await this.request({ type: "reset", reader })).atr;
  }

  /**
   * Reset every smart-card reader on USB, as if unplugged and plugged back in; returns their
   * names. Rejects with `unsupported` where the bridge cannot see USB.
   */
  async resetReaders(): Promise<string[]> {
    return (await this.request({ type: "reset_readers" })).reset;
  }
}
