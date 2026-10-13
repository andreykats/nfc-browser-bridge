// Pure helpers for the Write panel: form entries -> NDEF records, and capacity accounting.

import { encodeNdef, mimeRecord, textRecord, uriRecord, type NdefRecord, type TextEncoding } from "../../client/src/ndef.js";
import { maxMessageBytes } from "../../client/src/ntag.js";
import { encodeNdefTlv } from "../../client/src/tlv.js";

export type Entry =
  | { kind: "uri"; uri: string }
  | { kind: "text"; text: string; lang: string; encoding: TextEncoding }
  | { kind: "mime"; mimeType: string; payload: string };

export function entryToRecord(e: Entry): NdefRecord {
  switch (e.kind) {
    case "uri":
      if (!e.uri) throw new Error("URI is empty");
      return uriRecord(e.uri);
    case "text":
      if (!/^[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*$/.test(e.lang)) throw new Error(`Bad language code "${e.lang}"`);
      return textRecord(e.text, e.lang, e.encoding);
    case "mime":
      if (!/^[\w.+-]+\/[\w.+-]+$/.test(e.mimeType)) throw new Error(`Bad MIME type "${e.mimeType}"`);
      return mimeRecord(e.mimeType, e.payload);
  }
}

export interface Usage {
  /** Bytes the NDEF TLV (+ terminator when it fits) will occupy. */
  used: number;
  /** Data area bytes available from the TLV offset. */
  capacity: number;
  fits: boolean;
  /** Largest NDEF message that fits. */
  maxMessage: number;
}

export function usage(records: NdefRecord[], dataBytes: number, tlvOffset = 0): Usage {
  const tlv = encodeNdefTlv(encodeNdef(records)).length;
  const capacity = dataBytes - tlvOffset;
  return { used: tlv, capacity, fits: tlv <= capacity, maxMessage: maxMessageBytes(capacity) };
}

/** Parse the raw console: one APDU per line, spaces/colons allowed, # comments. */
export function parseApduLines(text: string): string[] {
  const out: string[] = [];
  for (const [n, raw] of text.split("\n").entries()) {
    const line = raw.replace(/#.*/, "").replace(/[\s:]/g, "").toUpperCase();
    if (!line) continue;
    if (!/^([0-9A-F]{2})+$/.test(line)) throw new Error(`Line ${n + 1}: not hex`);
    out.push(line);
  }
  return out;
}
