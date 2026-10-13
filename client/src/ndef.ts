// NDEF messages and records (NFC Forum NDEF 1.0, RTD URI, RTD Text).

export const TNF = {
  EMPTY: 0,
  WELL_KNOWN: 1,
  MEDIA: 2,
  ABSOLUTE_URI: 3,
  EXTERNAL: 4,
  UNKNOWN: 5,
  UNCHANGED: 6,
  RESERVED: 7,
} as const;

export const TNF_NAMES = ["Empty", "Well-known", "Media", "Absolute URI", "External", "Unknown", "Unchanged", "Reserved"];

export interface NdefRecord {
  tnf: number;
  type: Uint8Array;
  id: Uint8Array;
  payload: Uint8Array;
}

export class NdefError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NdefError";
  }
}

const MB = 0x80;
const ME = 0x40;
const CF = 0x20;
const SR = 0x10;
const IL = 0x08;

const utf8 = new TextEncoder();
const utf8d = new TextDecoder("utf-8", { fatal: true });

export function bytes(s: string): Uint8Array {
  return utf8.encode(s);
}

function str(b: Uint8Array): string {
  try {
    return utf8d.decode(b);
  } catch {
    throw new NdefError("Invalid UTF-8");
  }
}

// ---- Message encode / decode ----

export function parseNdef(data: Uint8Array): NdefRecord[] {
  const records: NdefRecord[] = [];
  let i = 0;
  if (data.length === 0) return records;
  for (;;) {
    const need = (n: number) => {
      if (i + n > data.length) throw new NdefError("Truncated NDEF record");
    };
    need(2);
    const header = data[i++];
    const tnf = header & 0x07;
    if (records.length === 0 && !(header & MB)) throw new NdefError("First record lacks the MB flag");
    if (records.length > 0 && header & MB) throw new NdefError("MB flag on a record after the first");
    if (header & CF || tnf === TNF.UNCHANGED) throw new NdefError("Chunked NDEF records are not supported");
    const typeLength = data[i++];
    let payloadLength: number;
    if (header & SR) {
      need(1);
      payloadLength = data[i++];
    } else {
      need(4);
      payloadLength = ((data[i] << 24) | (data[i + 1] << 16) | (data[i + 2] << 8) | data[i + 3]) >>> 0;
      i += 4;
    }
    let idLength = 0;
    if (header & IL) {
      need(1);
      idLength = data[i++];
    }
    need(typeLength + idLength + payloadLength);
    const type = data.slice(i, (i += typeLength));
    const id = data.slice(i, (i += idLength));
    const payload = data.slice(i, (i += payloadLength));
    if (tnf === TNF.EMPTY && (type.length || id.length || payload.length)) {
      throw new NdefError("Empty record with type, id or payload");
    }
    records.push({ tnf, type, id, payload });
    if (header & ME) return records;
    if (i >= data.length) throw new NdefError("Last record lacks the ME flag");
  }
}

export function encodeNdef(records: NdefRecord[]): Uint8Array {
  if (records.length === 0) return new Uint8Array(0);
  const parts: Uint8Array[] = [];
  records.forEach((r, n) => {
    if (r.tnf < 0 || r.tnf > 5) throw new NdefError(`Cannot encode TNF ${r.tnf}`);
    if (r.type.length > 255 || r.id.length > 255) throw new NdefError("Type or id longer than 255 bytes");
    const short = r.payload.length < 256;
    let header = r.tnf;
    if (n === 0) header |= MB;
    if (n === records.length - 1) header |= ME;
    if (short) header |= SR;
    if (r.id.length) header |= IL;
    const head = [header, r.type.length];
    if (short) head.push(r.payload.length);
    else head.push((r.payload.length >>> 24) & 0xff, (r.payload.length >>> 16) & 0xff, (r.payload.length >>> 8) & 0xff, r.payload.length & 0xff);
    if (r.id.length) head.push(r.id.length);
    parts.push(Uint8Array.from(head), r.type, r.id, r.payload);
  });
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

// ---- URI records ----

export const URI_PREFIXES = [
  "", "http://www.", "https://www.", "http://", "https://", "tel:", "mailto:", "ftp://anonymous:anonymous@",
  "ftp://ftp.", "ftps://", "sftp://", "smb://", "nfs://", "ftp://", "dav://", "news:", "telnet://", "imap:",
  "rtsp://", "urn:", "pop:", "sip:", "sips:", "tftp:", "btspp://", "btl2cap://", "btgoep://", "tcpobex://",
  "irdaobex://", "file://", "urn:epc:id:", "urn:epc:tag:", "urn:epc:pat:", "urn:epc:raw:", "urn:epc:", "urn:nfc:",
];

const TYPE_U = bytes("U");
const TYPE_T = bytes("T");

function eq(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

export function isUriRecord(r: NdefRecord): boolean {
  return r.tnf === TNF.WELL_KNOWN && eq(r.type, TYPE_U);
}

export function isTextRecord(r: NdefRecord): boolean {
  return r.tnf === TNF.WELL_KNOWN && eq(r.type, TYPE_T);
}

export function uriRecord(uri: string, id: Uint8Array = new Uint8Array(0)): NdefRecord {
  let code = 0;
  for (let c = 1; c < URI_PREFIXES.length; c++) {
    if (uri.startsWith(URI_PREFIXES[c]) && URI_PREFIXES[c].length > URI_PREFIXES[code].length) code = c;
  }
  const rest = bytes(uri.slice(URI_PREFIXES[code].length));
  const payload = new Uint8Array(1 + rest.length);
  payload[0] = code;
  payload.set(rest, 1);
  return { tnf: TNF.WELL_KNOWN, type: TYPE_U.slice(), id, payload };
}

export function decodeUri(r: NdefRecord): string {
  if (!isUriRecord(r)) throw new NdefError("Not a URI record");
  if (r.payload.length === 0) throw new NdefError("Empty URI record");
  // Codes past the table are RFU: no prefix.
  const prefix = URI_PREFIXES[r.payload[0]] ?? "";
  return prefix + str(r.payload.subarray(1));
}

// ---- Text records ----

export type TextEncoding = "utf-8" | "utf-16";

export interface TextContent {
  text: string;
  lang: string;
  encoding: TextEncoding;
}

function encodeUtf16be(s: string): Uint8Array {
  const out = new Uint8Array(s.length * 2);
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    out[2 * i] = c >> 8;
    out[2 * i + 1] = c & 0xff;
  }
  return out;
}

function decodeUtf16(b: Uint8Array): string {
  if (b.length % 2) throw new NdefError("Odd-length UTF-16 text");
  let le = false;
  let start = 0;
  if (b[0] === 0xff && b[1] === 0xfe) [le, start] = [true, 2];
  else if (b[0] === 0xfe && b[1] === 0xff) start = 2;
  let s = "";
  for (let i = start; i < b.length; i += 2) {
    s += String.fromCharCode(le ? b[i] | (b[i + 1] << 8) : (b[i] << 8) | b[i + 1]);
  }
  return s;
}

export function textRecord(text: string, lang = "en", encoding: TextEncoding = "utf-8", id: Uint8Array = new Uint8Array(0)): NdefRecord {
  const langBytes = bytes(lang);
  if (langBytes.length > 63) throw new NdefError("Language code longer than 63 bytes");
  const body = encoding === "utf-8" ? bytes(text) : encodeUtf16be(text);
  const payload = new Uint8Array(1 + langBytes.length + body.length);
  payload[0] = (encoding === "utf-16" ? 0x80 : 0) | langBytes.length;
  payload.set(langBytes, 1);
  payload.set(body, 1 + langBytes.length);
  return { tnf: TNF.WELL_KNOWN, type: TYPE_T.slice(), id, payload };
}

export function decodeText(r: NdefRecord): TextContent {
  if (!isTextRecord(r)) throw new NdefError("Not a Text record");
  if (r.payload.length === 0) throw new NdefError("Empty Text record");
  const status = r.payload[0];
  const langLength = status & 0x3f;
  if (1 + langLength > r.payload.length) throw new NdefError("Text record language code is truncated");
  const lang = str(r.payload.subarray(1, 1 + langLength));
  const body = r.payload.subarray(1 + langLength);
  const encoding: TextEncoding = status & 0x80 ? "utf-16" : "utf-8";
  return { text: encoding === "utf-16" ? decodeUtf16(body) : str(body), lang, encoding };
}

// ---- Other record types ----

export function mimeRecord(mimeType: string, data: Uint8Array | string, id: Uint8Array = new Uint8Array(0)): NdefRecord {
  return { tnf: TNF.MEDIA, type: bytes(mimeType), id, payload: typeof data === "string" ? bytes(data) : data };
}

export function externalRecord(type: string, data: Uint8Array, id: Uint8Array = new Uint8Array(0)): NdefRecord {
  return { tnf: TNF.EXTERNAL, type: bytes(type), id, payload: data };
}

export function absoluteUriRecord(uri: string, data: Uint8Array = new Uint8Array(0)): NdefRecord {
  return { tnf: TNF.ABSOLUTE_URI, type: bytes(uri), id: new Uint8Array(0), payload: data };
}

export function emptyRecord(): NdefRecord {
  return { tnf: TNF.EMPTY, type: new Uint8Array(0), id: new Uint8Array(0), payload: new Uint8Array(0) };
}

/**
 * The card identity the kiosk uses: the full URI of the first URI record, else the text (without
 * language) of the first Text record, else null.
 */
export function matchKey(records: NdefRecord[]): string | null {
  const uri = records.find(isUriRecord);
  if (uri) return decodeUri(uri);
  const text = records.find(isTextRecord);
  if (text) return decodeText(text).text;
  return null;
}

/** Human-readable decoding of one record, for display. */
export function describeRecord(r: NdefRecord): string {
  try {
    if (isUriRecord(r)) return decodeUri(r);
    if (isTextRecord(r)) {
      const t = decodeText(r);
      return `[${t.lang}, ${t.encoding}] ${t.text}`;
    }
    if (r.tnf === TNF.MEDIA && /^text\/|json|xml/.test(str(r.type))) return str(r.payload);
  } catch {
    // Fall through to "binary".
  }
  return r.payload.length ? `${r.payload.length} bytes` : "";
}
