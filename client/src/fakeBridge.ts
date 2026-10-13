// An in-memory Topps NFC Bridge speaking protocol v1 over a fake WebSocket, with NTAG213/215/216
// memory images. For tests: `new BridgeClient({ WebSocket: fake.WebSocket })`.
// Everything is delivered on microtasks, so it works with real and fake timers alike.

import type { WebSocketFactory, WebSocketLike } from "./client.js";
import { encodeNdef, type NdefRecord } from "./ndef.js";
import { fromHex, toHex } from "./ntag.js";
import { LIMITS, PROTOCOL_VERSION, type ErrorBody, type Reader, type ReaderKind, type ServerEvent } from "./protocol.js";
import { encodeNdefTlv } from "./tlv.js";

export type NtagKind = "NTAG213" | "NTAG215" | "NTAG216";

const PAGES: Record<NtagKind, number> = { NTAG213: 45, NTAG215: 135, NTAG216: 231 };
const CC_SIZE: Record<NtagKind, number> = { NTAG213: 0x12, NTAG215: 0x3e, NTAG216: 0x6d };

export const ATR_ULTRALIGHT = "3B8F8001804F0CA0000003060300030000000068";
export const ATR_CLASSIC_1K = "3B8F8001804F0CA000000306030001000000006A";

export class FakeTag {
  atr: string;
  uid: Uint8Array;
  memory: Uint8Array;
  /** Set by a failed command; every APDU answers 6F00 until reset. */
  wedged = false;

  constructor(atr: string, uid: Uint8Array, memory: Uint8Array) {
    this.atr = atr;
    this.uid = uid;
    this.memory = memory;
  }

  /** A blank NTAG with a valid CC and an empty NDEF TLV (factory state). */
  static ntag(kind: NtagKind, uid: Uint8Array = Uint8Array.from([0x04, 0x69, 0x9e, 0x22, 0xc0, 0x1c, 0x90])): FakeTag {
    const m = new Uint8Array(PAGES[kind] * 4);
    m.set(uid.subarray(0, 3), 0);
    m[3] = 0x88 ^ uid[0] ^ uid[1] ^ uid[2];
    m.set(uid.subarray(3, 7), 4);
    m[8] = uid[3] ^ uid[4] ^ uid[5] ^ uid[6];
    m[9] = 0x48;
    m.set([0xe1, 0x10, CC_SIZE[kind], 0x00], 12);
    m.set([0x03, 0x00, 0xfe], 16);
    return new FakeTag(ATR_ULTRALIGHT, uid, m);
  }

  /** A non-Type-2 card: answers FF CA only. */
  static classic1k(uid: Uint8Array = Uint8Array.from([0xde, 0xad, 0xbe, 0xef])): FakeTag {
    return new FakeTag(ATR_CLASSIC_1K, uid, new Uint8Array(0));
  }

  /** Put an NDEF message at page 4 (test setup helper). */
  withNdef(records: NdefRecord[]): this {
    const tlv = encodeNdefTlv(encodeNdef(records));
    this.memory.set(tlv, 16);
    if (16 + tlv.length < this.memory.length) this.memory[16 + tlv.length] = 0xfe;
    return this;
  }

  get uidHex(): string {
    return toHex(this.uid);
  }
}

interface FakeReader {
  name: string;
  kind: ReaderKind;
  tag: FakeTag | null;
  transparentSession: boolean;
}

class FakeError extends Error {
  constructor(readonly body: ErrorBody) {
    super(body.message);
  }
}

const OK = "9000";

export class FakeBridge {
  version = "0.0.0-fake";
  running = true;
  /** Hex APDUs in execution order. */
  readonly apduLog: string[] = [];
  /** If set, requests are never answered (timeout tests). */
  silent = false;
  readonly WebSocket: WebSocketFactory;

  private readonly readers: FakeReader[] = [];
  private readonly sockets = new Set<FakeSocket>();
  private failWrites = 0;
  private failWriteSw = "6300";
  private removeAfter: number | null = null;

  constructor() {
    const bridge = this;
    this.WebSocket = class extends FakeSocket {
      constructor(url: string) {
        super(bridge, url);
      }
    };
  }

  // ---- scripting ----

  addReader(name: string, kind: ReaderKind = "picc"): void {
    this.readers.push({ name, kind, tag: null, transparentSession: false });
    this.broadcastReaders();
  }

  removeReader(name: string): void {
    const i = this.readers.findIndex((r) => r.name === name);
    if (i < 0) return;
    if (this.readers[i].tag) this.broadcast({ event: "card_removed", reader: name });
    this.readers.splice(i, 1);
    this.broadcastReaders();
  }

  placeCard(reader: string, tag: FakeTag): void {
    const r = this.reader(reader);
    r.tag = tag;
    r.transparentSession = false;
    this.broadcast({ event: "card_present", reader, uid: tag.uidHex, atr: tag.atr });
    this.broadcastReaders();
  }

  removeCard(reader: string): FakeTag | null {
    const r = this.reader(reader);
    const tag = r.tag;
    if (!tag) return null;
    r.tag = null;
    this.broadcast({ event: "card_removed", reader });
    this.broadcastReaders();
    return tag;
  }

  tag(reader: string): FakeTag | null {
    return this.reader(reader).tag;
  }

  /** The next `count` FF D6 writes answer `sw` without writing (like the ACR1552's flaky writes). */
  failNextWrites(count: number, sw = "6300"): void {
    this.failWrites = count;
    this.failWriteSw = sw;
  }

  /** The card leaves the field after `n` more APDUs execute (mid-batch). */
  removeCardAfterApdus(n: number): void {
    this.removeAfter = n;
  }

  /** Simulate quitting the bridge: drop all connections and refuse new ones. */
  stop(): void {
    this.running = false;
    for (const s of this.sockets) s.serverClose();
    this.sockets.clear();
  }

  start(): void {
    this.running = true;
  }

  get connections(): number {
    return this.sockets.size;
  }

  // ---- server side ----

  private reader(name: string): FakeReader {
    const r = this.readers.find((x) => x.name === name);
    if (!r) throw new Error(`No fake reader ${name}`);
    return r;
  }

  private publicReaders(): Reader[] {
    return this.readers.map((r) => ({
      name: r.name,
      kind: r.kind,
      card: r.tag ? { uid: r.tag.uidHex, atr: r.tag.atr } : null,
    }));
  }

  private broadcast(ev: ServerEvent): void {
    for (const s of this.sockets) s.deliver(ev);
  }

  private broadcastReaders(): void {
    this.broadcast({ event: "readers", service: "ready", readers: this.publicReaders() });
  }

  /** @internal */
  connected(s: FakeSocket): void {
    this.sockets.add(s);
    s.deliver({
      event: "hello",
      protocol: PROTOCOL_VERSION,
      app: "topps-nfc-bridge",
      version: this.version,
      service: "ready",
      readers: this.publicReaders(),
    });
  }

  /** @internal */
  disconnected(s: FakeSocket): void {
    this.sockets.delete(s);
  }

  /** @internal */
  handle(s: FakeSocket, text: string): void {
    if (this.silent) return;
    let req: Record<string, unknown>;
    try {
      req = JSON.parse(text);
    } catch {
      s.deliver({ id: null, ok: false, error: { code: "bad_request", message: "Invalid JSON" } });
      return;
    }
    const id = req.id ?? null;
    try {
      s.deliver({ id, ok: true, result: this.execute(req) });
    } catch (e) {
      if (!(e instanceof FakeError)) throw e;
      s.deliver({ id, ok: false, error: e.body });
    }
  }

  private execute(req: Record<string, unknown>): unknown {
    const fail = (code: ErrorBody["code"], message: string, extra: Partial<ErrorBody> = {}): never => {
      throw new FakeError({ code, message, ...extra });
    };
    switch (req.type) {
      case "ping":
        return {};
      case "list_readers":
        return { service: "ready", readers: this.publicReaders() };
      case "reset": {
        const r = this.readers.find((x) => x.name === req.reader) ?? fail("no_such_reader", "No such reader");
        const tag = r.tag ?? fail("no_card", "No card on reader");
        tag.wedged = false;
        r.transparentSession = false;
        return { atr: tag.atr };
      }
      case "transmit": {
        const apdus = req.apdus as string[];
        if (!Array.isArray(apdus) || apdus.length === 0 || apdus.length > LIMITS.maxApdusPerBatch) {
          fail("bad_request", "Bad apdus");
        }
        const bytes = apdus.map((a) => {
          let b: Uint8Array;
          try {
            b = fromHex(a);
          } catch {
            return fail("bad_request", "Bad hex");
          }
          if (b.length < LIMITS.minApduBytes || b.length > LIMITS.maxApduBytes) fail("bad_request", "Bad APDU length");
          if (!req.unsafe && isGuardedWrite(b)) fail("blocked_write", "Write to pages 0-3");
          return b;
        });
        const r = this.readers.find((x) => x.name === req.reader) ?? fail("no_such_reader", "No such reader");
        if (!r.tag) fail("no_card", "No card on reader", { pcsc: "0x8010000C" });
        const stopOnError = req.stopOnError !== false;
        const responses: { data: string; sw: string }[] = [];
        for (const apdu of bytes) {
          if (this.removeAfter !== null && this.removeAfter-- <= 0) {
            this.removeAfter = null;
            this.removeCard(r.name);
          }
          if (!r.tag) fail("card_removed", "Card was removed", { pcsc: "0x80100069", completed: responses.length });
          this.apduLog.push(toHex(apdu));
          const resp = this.respond(r, apdu);
          responses.push(resp);
          if (stopOnError && resp.sw !== OK) break;
        }
        return { responses };
      }
      default:
        return fail("bad_request", `Unknown type ${String(req.type)}`);
    }
  }

  private respond(r: FakeReader, a: Uint8Array): { data: string; sw: string } {
    const tag = r.tag!;
    const sw = (s: string, data = "") => ({ data, sw: s });
    if (tag.wedged) return sw("6F00");
    const isNtag = tag.memory.length > 0;
    if (a[0] !== 0xff) return sw("6E00");
    switch (a[1]) {
      case 0xca:
        return sw(OK, toHex(tag.uid));
      case 0xb0: {
        if (!isNtag) return sw("6981");
        const start = ((a[2] << 8) | a[3]) * 4;
        if (start >= tag.memory.length) {
          tag.wedged = true;
          return sw("6300");
        }
        const out = Array.from({ length: a[4] }, (_, i) => tag.memory[(start + i) % tag.memory.length]);
        return sw(OK, toHex(out));
      }
      case 0xd6: {
        if (!isNtag) return sw("6981");
        if (this.failWrites > 0) {
          this.failWrites--;
          return sw(this.failWriteSw);
        }
        return this.write(tag, (a[2] << 8) | a[3], a.subarray(5, 5 + a[4])) ? sw(OK) : sw("6300");
      }
      case 0xc2: {
        if (a[3] === 0x00 && a[5] === 0x81) {
          r.transparentSession = true;
          return sw(OK, "C003009000");
        }
        if (a[3] === 0x00 && a[5] === 0x82) {
          r.transparentSession = false;
          return sw(OK, "C003009000");
        }
        // 95 06 A2 page d0..d3
        if (a[3] === 0x01 && r.transparentSession && a[5] === 0x95 && a[7] === 0xa2) {
          return this.write(tag, a[8], a.subarray(9, 13)) ? sw(OK, "C0030090009201009602000097010A") : sw("6300");
        }
        return sw("6A81");
      }
      default:
        return sw("6A81");
    }
  }

  private write(tag: FakeTag, page: number, data: Uint8Array): boolean {
    if (data.length !== 4 || (page + 1) * 4 > tag.memory.length) {
      tag.wedged = true;
      return false;
    }
    tag.memory.set(data, page * 4);
    return true;
  }
}

function isGuardedWrite(a: Uint8Array): boolean {
  if (a[0] === 0xff && a[1] === 0xd6) return ((a[2] << 8) | a[3]) < 4;
  if (a[0] === 0xff && a[1] === 0xc2 && a[3] === 0x01 && a[5] === 0x95) return (a[7] === 0xa2 || a[7] === 0xa0) && a[8] < 4;
  return false;
}

const CONNECTING = 0;
const OPEN = 1;
const CLOSED = 3;

class FakeSocket implements WebSocketLike {
  readyState = CONNECTING;
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;

  constructor(
    private readonly bridge: FakeBridge,
    readonly url: string,
  ) {
    queueMicrotask(() => {
      if (this.readyState !== CONNECTING) return;
      if (!bridge.running) {
        this.readyState = CLOSED;
        this.onerror?.({});
        this.onclose?.({ code: 1006 });
        return;
      }
      this.readyState = OPEN;
      this.onopen?.({});
      bridge.connected(this);
    });
  }

  send(data: string): void {
    if (this.readyState !== OPEN) throw new Error("WebSocket is not open");
    queueMicrotask(() => {
      if (this.readyState === OPEN) this.bridge.handle(this, data);
    });
  }

  close(): void {
    if (this.readyState === CLOSED) return;
    this.readyState = CLOSED;
    this.bridge.disconnected(this);
    queueMicrotask(() => this.onclose?.({ code: 1000 }));
  }

  /** @internal Bridge-side close (bridge quit). */
  serverClose(): void {
    if (this.readyState === CLOSED) return;
    this.readyState = CLOSED;
    queueMicrotask(() => this.onclose?.({ code: 1006 }));
  }

  /** @internal */
  deliver(msg: unknown): void {
    const data = JSON.stringify(msg);
    queueMicrotask(() => {
      if (this.readyState === OPEN) this.onmessage?.({ data });
    });
  }
}
