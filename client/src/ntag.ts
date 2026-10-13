// NTAG213/215/216 (NFC Forum Type 2) NDEF read/write over the bridge's raw APDU pass-through,
// using the PC/SC Part 3 storage-card commands (FF B0 read, FF D6 write).

import { NdefError, encodeNdef, parseNdef, type NdefRecord } from "./ndef.js";
import type { ApduResponse } from "./protocol.js";
import { TlvError, encodeNdefTlv, locateNdef } from "./tlv.js";

/** What the NTAG functions need from the bridge. `BridgeClient` implements it. */
export interface CardTransport {
  transmit(reader: string, apdus: string[], options?: { stopOnError?: boolean; unsafe?: boolean }): Promise<ApduResponse[]>;
  reset(reader: string): Promise<string>;
}

export type NtagErrorCode = "not_type2" | "not_ndef" | "read_only" | "capacity" | "apdu_failed" | "corrupt" | "verify_failed";

export class NtagError extends Error {
  readonly code: NtagErrorCode;
  readonly sw?: string;
  /** For `capacity`: TLV bytes needed and available. */
  readonly needed?: number;
  readonly available?: number;

  constructor(code: NtagErrorCode, message: string, extra: { sw?: string; needed?: number; available?: number } = {}) {
    super(message);
    this.name = "NtagError";
    this.code = code;
    Object.assign(this, extra);
  }
}

export type NtagModel = "NTAG213" | "NTAG215" | "NTAG216" | "Type 2";

export interface CapabilityContainer {
  /** CC bytes (page 3) as hex. */
  raw: string;
  model: NtagModel;
  /** Size of the NDEF data area in bytes (from the CC). */
  dataBytes: number;
  /** User memory pages: firstPage..lastPage inclusive. */
  firstPage: number;
  lastPage: number;
  readable: boolean;
  writable: boolean;
}

export interface NdefContent {
  cc: CapabilityContainer;
  /** Offset of the NDEF TLV in the data area (or where one would be written). */
  tlvOffset: number;
  /** Raw NDEF message bytes (empty if the tag holds no NDEF message). */
  message: Uint8Array;
  records: NdefRecord[];
}

export interface WriteResult {
  pagesWritten: number;
  /** TLV bytes used, including the terminator if written. */
  bytesUsed: number;
  /** True if the plain write failed and the ACR1552 transparent session was used. */
  usedTransparentSession: boolean;
}

const FIRST_DATA_PAGE = 4;
const SIZES: Record<number, NtagModel> = { 0x12: "NTAG213", 0x3e: "NTAG215", 0x6d: "NTAG216" };
const PCSC_RID_AID = [0x4f, 0x0c, 0xa0, 0x00, 0x00, 0x03, 0x06];

// ---- hex ----

export function toHex(b: Uint8Array | number[]): string {
  return Array.from(b, (x) => x.toString(16).toUpperCase().padStart(2, "0")).join("");
}

export function fromHex(s: string): Uint8Array {
  if (s.length % 2 || /[^0-9a-f]/i.test(s)) throw new Error(`Invalid hex: ${s}`);
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

// ---- detection ----

/**
 * True if the ATR is a PC/SC Part 3 contactless storage card ATR with card name 00 03
 * (MIFARE Ultralight family, which includes NTAG21x).
 */
export function isType2(atrHex: string): boolean {
  let atr: Uint8Array;
  try {
    atr = fromHex(atrHex);
  } catch {
    return false;
  }
  for (let i = 0; i + PCSC_RID_AID.length + 3 <= atr.length; i++) {
    if (PCSC_RID_AID.every((b, k) => atr[i + k] === b)) {
      const name = atr.subarray(i + PCSC_RID_AID.length + 1, i + PCSC_RID_AID.length + 3);
      return name[0] === 0x00 && name[1] === 0x03;
    }
  }
  return false;
}

function requireType2(atr: string): void {
  if (!isType2(atr)) throw new NtagError("not_type2", "Not an NFC Forum Type 2 (NTAG/Ultralight) card");
}

// ---- APDU helpers ----

function readApdus(firstPage: number, pages: number): string[] {
  const out: string[] = [];
  for (let p = firstPage; p < firstPage + pages; p += 4) {
    const n = Math.min(4, firstPage + pages - p);
    out.push(toHex([0xff, 0xb0, p >> 8, p & 0xff, n * 4]));
  }
  return out;
}

function writeApdu(page: number, data: Uint8Array): string {
  return toHex([0xff, 0xd6, page >> 8, page & 0xff, 4, ...data]);
}

const SESSION_START = "FFC20000028100";
const SESSION_END = "FFC20000028200";

function transparentWriteApdu(page: number, data: Uint8Array): string {
  // Transceive data object (95) carrying a Type 2 WRITE (A2 page d0..d3).
  return toHex([0xff, 0xc2, 0x00, 0x01, 0x08, 0x95, 0x06, 0xa2, page, ...data]);
}

function requireOk(responses: ApduResponse[], expected: number): void {
  const bad = responses.find((r) => r.sw !== "9000");
  if (bad) throw new NtagError("apdu_failed", `Card answered ${bad.sw}`, { sw: bad.sw });
  if (responses.length !== expected) throw new NtagError("apdu_failed", "Batch stopped early");
}

function concat(responses: ApduResponse[]): Uint8Array {
  return fromHex(responses.map((r) => r.data).join(""));
}

// ---- CC ----

export function parseCC(cc: Uint8Array): CapabilityContainer {
  if (cc.length < 4 || cc[0] !== 0xe1 || cc[1] >> 4 !== 1) {
    throw new NtagError("not_ndef", "Card is not NDEF formatted (no valid capability container)");
  }
  const dataBytes = cc[2] * 8;
  return {
    raw: toHex(cc.subarray(0, 4)),
    model: SIZES[cc[2]] ?? "Type 2",
    dataBytes,
    firstPage: FIRST_DATA_PAGE,
    lastPage: FIRST_DATA_PAGE + dataBytes / 4 - 1,
    readable: cc[3] >> 4 === 0,
    writable: (cc[3] & 0x0f) === 0,
  };
}

export async function readCC(t: CardTransport, reader: string, atr: string): Promise<CapabilityContainer> {
  requireType2(atr);
  const r = await t.transmit(reader, [toHex([0xff, 0xb0, 0x00, 0x03, 0x04])]);
  requireOk(r, 1);
  return parseCC(fromHex(r[0].data));
}

/** One batch: the CC plus the first 16 bytes of the data area. */
async function readHead(t: CardTransport, reader: string): Promise<{ cc: CapabilityContainer; data: Uint8Array }> {
  const r = await t.transmit(reader, [toHex([0xff, 0xb0, 0x00, 0x03, 0x04]), ...readApdus(FIRST_DATA_PAGE, 4)]);
  requireOk(r, 2);
  const cc = parseCC(fromHex(r[0].data));
  return { cc, data: fromHex(r[1].data).subarray(0, cc.dataBytes) };
}

/** Extend `data` (bytes from the start of the data area) to at least `bytes` bytes in one batch. */
async function readMore(t: CardTransport, reader: string, cc: CapabilityContainer, data: Uint8Array, bytes: number): Promise<Uint8Array> {
  const want = Math.min(bytes, cc.dataBytes);
  if (want <= data.length) return data;
  const havePages = Math.floor(data.length / 4);
  const wantPages = Math.ceil(want / 4);
  const apdus = readApdus(FIRST_DATA_PAGE + havePages, wantPages - havePages);
  const r = await t.transmit(reader, apdus);
  requireOk(r, apdus.length);
  const out = new Uint8Array(wantPages * 4);
  out.set(data.subarray(0, havePages * 4));
  out.set(concat(r).subarray(0, (wantPages - havePages) * 4), havePages * 4);
  return out;
}

/** Find the NDEF TLV, reading further only as far as needed. Garbage (non-TLV) data counts as no NDEF. */
async function locate(t: CardTransport, reader: string, cc: CapabilityContainer, data: Uint8Array) {
  for (;;) {
    let loc;
    try {
      loc = locateNdef(data, cc.dataBytes);
    } catch (e) {
      if (e instanceof TlvError) return { data, loc: null, error: e };
      throw e;
    }
    if (loc.kind !== "need") return { data, loc, error: null };
    // Read generously: the rest of a 16-byte block past what is needed.
    data = await readMore(t, reader, cc, data, Math.ceil(loc.bytes / 16) * 16);
  }
}

// ---- read ----

export async function readNdef(t: CardTransport, reader: string, atr: string): Promise<NdefContent> {
  requireType2(atr);
  const head = await readHead(t, reader);
  const { cc } = head;
  if (!cc.readable) throw new NtagError("read_only", "Card denies read access");
  const { data, loc, error } = await locate(t, reader, cc, head.data);
  if (error) throw new NtagError("corrupt", error.message);
  if (!loc || loc.kind !== "found") {
    return { cc, tlvOffset: loc?.kind === "none" ? loc.offset : 0, message: new Uint8Array(0), records: [] };
  }
  const full = await readMore(t, reader, cc, data, loc.valueOffset + loc.length);
  const message = full.slice(loc.valueOffset, loc.valueOffset + loc.length);
  try {
    return { cc, tlvOffset: loc.offset, message, records: parseNdef(message) };
  } catch (e) {
    if (e instanceof NdefError) throw new NtagError("corrupt", e.message);
    throw e;
  }
}

// ---- write ----

/** Bytes an NDEF message needs on the tag: TLV header + message (terminator not counted). */
export function tlvSize(message: Uint8Array): number {
  return encodeNdefTlv(message).length;
}

/** Largest NDEF message that fits in `available` data-area bytes. */
export function maxMessageBytes(available: number): number {
  const short = Math.min(available - 2, 0xfe);
  const long = available - 4 >= 0xff ? available - 4 : -1;
  return Math.max(short, long, 0);
}

interface PageWrite {
  page: number;
  data: Uint8Array;
}

/**
 * Plan the page writes so that an interruption at any point leaves either the old content or an
 * empty-but-valid NDEF TLV:
 *   1. header pages with an empty TLV (03 00 FE), in order;
 *   2. the remaining pages with their final content;
 *   3. header pages with the real length, last page first.
 */
export function planWrite(existing: Uint8Array, tlvOffset: number, message: Uint8Array, dataBytes: number): PageWrite[] {
  const tlv = encodeNdefTlv(message);
  const headerLength = tlv.length - message.length;
  const end = tlvOffset + tlv.length;
  if (end > dataBytes) {
    throw new NtagError("capacity", `Message needs ${tlv.length} bytes; ${dataBytes - tlvOffset} available`, {
      needed: tlv.length,
      available: dataBytes - tlvOffset,
    });
  }
  const regionEnd = end < dataBytes ? end + 1 : end; // room for a terminator?
  const firstIdx = Math.floor(tlvOffset / 4);
  const lastIdx = Math.ceil(regionEnd / 4) - 1;
  const base = firstIdx * 4;

  const final = new Uint8Array((lastIdx - firstIdx + 1) * 4);
  final.set(existing.subarray(base, tlvOffset)); // keep control TLVs sharing the first page
  final.set(tlv, tlvOffset - base);
  if (regionEnd > end) final[end - base] = 0xfe;

  const headerLastIdx = Math.floor((tlvOffset + headerLength - 1) / 4);
  const empty = final.slice(0, (headerLastIdx - firstIdx + 1) * 4);
  empty.fill(0, tlvOffset - base);
  empty.set([0x03, 0x00, 0xfe].slice(0, empty.length - (tlvOffset - base)), tlvOffset - base);

  const page = (idx: number, img: Uint8Array) => ({
    page: FIRST_DATA_PAGE + idx,
    data: img.slice((idx - firstIdx) * 4, (idx - firstIdx) * 4 + 4),
  });
  const writes: PageWrite[] = [];
  for (let i = firstIdx; i <= headerLastIdx; i++) writes.push(page(i, empty));
  for (let i = headerLastIdx + 1; i <= lastIdx; i++) writes.push(page(i, final));
  for (let i = headerLastIdx; i >= firstIdx; i--) {
    const w = page(i, final);
    if (toHex(w.data) !== toHex(page(i, empty).data)) writes.push(w);
  }
  return writes;
}

const CARD_GONE = new Set(["card_removed", "no_card", "no_such_reader", "pcsc_unavailable", "disconnected"]);

function isCardGone(e: unknown): boolean {
  return typeof e === "object" && e !== null && CARD_GONE.has((e as { code?: string }).code ?? "");
}

/**
 * Write an NDEF message (records or raw message bytes). Keeps any lock/memory control TLVs in front
 * of the NDEF TLV, never touches pages outside the CC's data area, and verifies by reading back.
 */
export async function writeNdef(
  t: CardTransport,
  reader: string,
  atr: string,
  content: NdefRecord[] | Uint8Array,
): Promise<WriteResult> {
  requireType2(atr);
  const message = content instanceof Uint8Array ? content : encodeNdef(content);
  const head = await readHead(t, reader);
  const { cc } = head;
  if (!cc.writable) throw new NtagError("read_only", "Card is write-protected (CC access byte)");
  const { data, loc } = await locate(t, reader, cc, head.data);
  // Garbage in the data area: start over at offset 0.
  const tlvOffset = loc ? loc.offset : 0;
  const writes = planWrite(data, tlvOffset, message, cc.dataBytes);
  for (const w of writes) {
    if (w.page < cc.firstPage || w.page > cc.lastPage) throw new Error(`Refusing to write page ${w.page}`);
  }

  let usedTransparentSession = false;
  try {
    const r = await t.transmit(reader, writes.map((w) => writeApdu(w.page, w.data)));
    requireOk(r, writes.length);
  } catch (e) {
    if (isCardGone(e)) throw e;
    // Assume a partial write. Recover the reader, then retry through the ACS transparent session.
    await t.reset(reader).catch(() => {});
    usedTransparentSession = true;
    const apdus = [SESSION_START, ...writes.map((w) => transparentWriteApdu(w.page, w.data)), SESSION_END];
    const r = await t.transmit(reader, apdus);
    if (r.length !== apdus.length || r.some((x) => x.sw !== "9000")) {
      await t.transmit(reader, [SESSION_END], { stopOnError: false }).catch(() => {});
      await t.reset(reader).catch(() => {});
      throw e;
    }
  }

  // Verify.
  const first = writes.reduce((m, w) => Math.min(m, w.page), Infinity);
  const last = writes.reduce((m, w) => Math.max(m, w.page), -Infinity);
  const r = await t.transmit(reader, readApdus(first, last - first + 1));
  requireOk(r, Math.ceil((last - first + 1) / 4));
  const back = concat(r);
  const expected = new Map<number, string>();
  for (const w of writes) expected.set(w.page, toHex(w.data)); // later writes win
  for (const [page, hex] of expected) {
    const off = (page - first) * 4;
    if (toHex(back.subarray(off, off + 4)) !== hex) {
      throw new NtagError("verify_failed", `Page ${page} reads back differently`);
    }
  }
  const end = tlvOffset + encodeNdefTlv(message).length;
  return { pagesWritten: writes.length, bytesUsed: end < cc.dataBytes ? end - tlvOffset + 1 : end - tlvOffset, usedTransparentSession };
}

/** Replace the NDEF message with an empty one (03 00 FE). */
export function eraseNdef(t: CardTransport, reader: string, atr: string): Promise<WriteResult> {
  return writeNdef(t, reader, atr, new Uint8Array(0));
}
