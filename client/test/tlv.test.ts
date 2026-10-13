import { describe, expect, it } from "vitest";
import { TlvError, encodeNdefTlv, locateNdef } from "../src/tlv.js";
import { fromHex, toHex } from "../src/ntag.js";

describe("locateNdef", () => {
  it("finds an NDEF TLV after NULL padding and control TLVs", () => {
    const data = fromHex("0000" + "0103A00C34" + "02030000FF" + "0305D1010155" + "00FE");
    expect(locateNdef(data, 144)).toEqual({ kind: "found", offset: 12, headerLength: 2, valueOffset: 14, length: 5 });
  });

  it("reads the 3-byte length form", () => {
    const data = fromHex("03FF0123");
    expect(locateNdef(data, 496)).toEqual({ kind: "found", offset: 0, headerLength: 4, valueOffset: 4, length: 0x123 });
  });

  it("asks for more bytes when a header is cut off", () => {
    expect(locateNdef(fromHex("0000"), 144)).toEqual({ kind: "need", bytes: 3 });
    expect(locateNdef(fromHex("03"), 144)).toEqual({ kind: "need", bytes: 2 });
    expect(locateNdef(fromHex("03FF01"), 144)).toEqual({ kind: "need", bytes: 4 });
    // Skipping a lock control TLV whose value runs past what we have.
    expect(locateNdef(fromHex("0103A0"), 144)).toEqual({ kind: "need", bytes: 6 });
  });

  it("reports no NDEF at the terminator, with the insertion point after control TLVs", () => {
    expect(locateNdef(fromHex("FE"), 144)).toEqual({ kind: "none", offset: 0 });
    expect(locateNdef(fromHex("0103A00C34FE"), 144)).toEqual({ kind: "none", offset: 5 });
  });

  it("finds an empty NDEF TLV", () => {
    expect(locateNdef(fromHex("0300FE"), 144)).toEqual({ kind: "found", offset: 0, headerLength: 2, valueOffset: 2, length: 0 });
  });

  it("rejects TLVs that overrun the data area and unknown types", () => {
    expect(() => locateNdef(fromHex("0310"), 16)).toThrow(TlvError);
    expect(() => locateNdef(fromHex("4401"), 144)).toThrow(/Unknown TLV/);
  });
});

describe("encodeNdefTlv", () => {
  it("switches to the 3-byte length at 255", () => {
    expect(toHex(encodeNdefTlv(new Uint8Array(254)).subarray(0, 2))).toBe("03FE");
    expect(toHex(encodeNdefTlv(new Uint8Array(255)).subarray(0, 4))).toBe("03FF00FF");
    expect(encodeNdefTlv(new Uint8Array(255)).length).toBe(259);
    expect(toHex(encodeNdefTlv(new Uint8Array()))).toBe("0300");
  });
});
