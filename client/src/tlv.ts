// NFC Forum Type 2 Tag TLV blocks (data area starting at page 4).

export const TLV_NULL = 0x00;
export const TLV_LOCK_CONTROL = 0x01;
export const TLV_MEMORY_CONTROL = 0x02;
export const TLV_NDEF = 0x03;
export const TLV_PROPRIETARY = 0xfd;
export const TLV_TERMINATOR = 0xfe;

export class TlvError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TlvError";
  }
}

export type NdefLocation =
  /** NDEF TLV found. `offset` is where the TLV starts, `valueOffset` where the message starts. */
  | { kind: "found"; offset: number; headerLength: number; valueOffset: number; length: number }
  /** Need at least `bytes` bytes from the start of the data area to decide. */
  | { kind: "need"; bytes: number }
  /** No NDEF TLV before the terminator / end of data. `offset` is where one could be written. */
  | { kind: "none"; offset: number };

/**
 * Walk TLVs in `data` (the first `data.length` bytes of a data area of `areaSize` bytes) looking
 * for the first NDEF TLV. Lock/memory control and proprietary TLVs are skipped; NULL TLVs are padding.
 */
export function locateNdef(data: Uint8Array, areaSize: number): NdefLocation {
  let i = 0;
  // Where an NDEF TLV could go: after the last control TLV.
  let insertAt = 0;
  while (i < areaSize) {
    if (i >= data.length) return { kind: "need", bytes: i + 1 };
    const type = data[i];
    if (type === TLV_NULL) {
      i += 1;
      continue;
    }
    if (type === TLV_TERMINATOR) return { kind: "none", offset: insertAt };
    if (i + 1 >= data.length) return { kind: "need", bytes: i + 2 };
    let length = data[i + 1];
    let headerLength = 2;
    if (length === 0xff) {
      if (i + 3 >= data.length) return { kind: "need", bytes: i + 4 };
      length = (data[i + 2] << 8) | data[i + 3];
      headerLength = 4;
    }
    const valueOffset = i + headerLength;
    if (valueOffset + length > areaSize) {
      throw new TlvError(`TLV 0x${hex2(type)} at ${i} runs past the end of the data area`);
    }
    if (type === TLV_NDEF) return { kind: "found", offset: i, headerLength, valueOffset, length };
    if (type !== TLV_LOCK_CONTROL && type !== TLV_MEMORY_CONTROL && type !== TLV_PROPRIETARY) {
      throw new TlvError(`Unknown TLV type 0x${hex2(type)} at ${i}`);
    }
    i = valueOffset + length;
    insertAt = i;
  }
  return { kind: "none", offset: insertAt };
}

/** Encode `03 len message`, with the 3-byte `FF hi lo` length for messages of 255 bytes or more. */
export function encodeNdefTlv(message: Uint8Array): Uint8Array {
  if (message.length > 0xfffe) throw new TlvError("NDEF message too long for a TLV");
  const header =
    message.length < 0xff ? [TLV_NDEF, message.length] : [TLV_NDEF, 0xff, message.length >> 8, message.length & 0xff];
  const out = new Uint8Array(header.length + message.length);
  out.set(header, 0);
  out.set(message, header.length);
  return out;
}

function hex2(n: number): string {
  return n.toString(16).toUpperCase().padStart(2, "0");
}
