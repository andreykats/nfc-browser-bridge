import { describe, expect, it } from "vitest";
import {
  NdefError,
  TNF,
  absoluteUriRecord,
  bytes,
  decodeText,
  decodeUri,
  describeRecord,
  emptyRecord,
  encodeNdef,
  externalRecord,
  matchKey,
  mimeRecord,
  parseNdef,
  textRecord,
  uriRecord,
  URI_PREFIXES,
  type NdefRecord,
} from "../src/ndef.js";
import { fromHex, toHex } from "../src/ntag.js";

const roundTrip = (records: NdefRecord[]) => parseNdef(encodeNdef(records));

describe("NDEF records", () => {
  it("round-trips URIs with every prefix, picking the longest", () => {
    for (let code = 1; code < URI_PREFIXES.length; code++) {
      const uri = URI_PREFIXES[code] + "example/x?y=1";
      const [r] = roundTrip([uriRecord(uri)]);
      expect(decodeUri(r)).toBe(uri);
    }
    expect(uriRecord("https://www.topps.com").payload[0]).toBe(0x02);
    expect(uriRecord("https://topps.drey.dev/c/1").payload[0]).toBe(0x04);
    expect(uriRecord("urn:epc:id:sgtin:1").payload[0]).toBe(0x1e);
    expect(uriRecord("custom:thing").payload[0]).toBe(0x00);
  });

  it("encodes a known URI record byte-for-byte", () => {
    // D1 01 0B 55 04 "topps.com/x" … well-known, SR, MB+ME
    expect(toHex(encodeNdef([uriRecord("https://topps.com/x")]))).toBe("D1010C5504" + toHex(bytes("topps.com/x")));
  });

  it("treats RFU URI prefix codes as no prefix", () => {
    const r: NdefRecord = { tnf: TNF.WELL_KNOWN, type: bytes("U"), id: new Uint8Array(), payload: Uint8Array.from([0x50, 0x61]) };
    expect(decodeUri(r)).toBe("a");
  });

  it("round-trips Text in UTF-8 and UTF-16 with a language", () => {
    for (const encoding of ["utf-8", "utf-16"] as const) {
      const [r] = roundTrip([textRecord("Grüße 🃏 card", "de-DE", encoding)]);
      expect(decodeText(r)).toEqual({ text: "Grüße 🃏 card", lang: "de-DE", encoding });
    }
  });

  it("decodes UTF-16 text with either BOM", () => {
    const mk = (body: number[]): NdefRecord => ({
      tnf: TNF.WELL_KNOWN,
      type: bytes("T"),
      id: new Uint8Array(),
      payload: Uint8Array.from([0x82, 0x65, 0x6e, ...body]),
    });
    expect(decodeText(mk([0xff, 0xfe, 0x48, 0x00, 0x69, 0x00])).text).toBe("Hi");
    expect(decodeText(mk([0xfe, 0xff, 0x00, 0x48, 0x00, 0x69])).text).toBe("Hi");
  });

  it("round-trips every TNF, ids, and long records in a multi-record message", () => {
    const long = new Uint8Array(70_000).map((_, i) => i & 0xff);
    const records = [
      uriRecord("https://topps.drey.dev/card/42", bytes("id1")),
      textRecord("hello", "en"),
      mimeRecord("application/json", '{"a":1}'),
      mimeRecord("application/octet-stream", long),
      externalRecord("drey.dev:card", Uint8Array.from([1, 2, 3])),
      absoluteUriRecord("https://example.com/schema"),
      emptyRecord(),
      { tnf: TNF.UNKNOWN, type: new Uint8Array(), id: new Uint8Array(), payload: Uint8Array.from([9]) },
    ];
    const encoded = encodeNdef(records);
    const back = parseNdef(encoded);
    expect(back).toEqual(records);
    // Flags: MB on first only, ME on last only, SR off for the 70 kB record.
    const headers: number[] = [];
    let i = 0;
    for (const r of back) {
      headers.push(encoded[i]);
      const sr = encoded[i] & 0x10;
      const il = encoded[i] & 0x08;
      i += 2 + (sr ? 1 : 4) + (il ? 1 : 0) + r.type.length + r.id.length + r.payload.length;
    }
    expect(headers.map((h) => h & 0xc0)).toEqual([0x80, 0, 0, 0, 0, 0, 0, 0x40]);
    expect(headers[3] & 0x10).toBe(0);
    expect(headers[0] & 0x08).toBe(0x08);
  });

  it("uses the long form at exactly 256 payload bytes", () => {
    const r = mimeRecord("x/y", new Uint8Array(255));
    expect(encodeNdef([r])[0] & 0x10).toBe(0x10);
    const r2 = mimeRecord("x/y", new Uint8Array(256));
    expect(encodeNdef([r2])[0] & 0x10).toBe(0);
    expect(roundTrip([r2])).toEqual([r2]);
  });

  it("parses an empty message as no records", () => {
    expect(parseNdef(new Uint8Array())).toEqual([]);
    expect(encodeNdef([])).toEqual(new Uint8Array());
  });

  it("rejects chunked, truncated and malformed messages", () => {
    expect(() => parseNdef(fromHex("B50100"))).toThrow(/Chunked/); // MB|CF|SR, TNF 5
    expect(() => parseNdef(fromHex("D1010C5504746F70"))).toThrow(/Truncated/);
    expect(() => parseNdef(fromHex("91010155" + "00"))).toThrow(/ME/); // MB without ME, then end
    expect(() => parseNdef(fromHex("51010155" + "00"))).toThrow(/MB/);
    expect(() => parseNdef(fromHex("D0000100" + "00"))).toThrow(NdefError);
  });
});

describe("matchKey", () => {
  it("prefers the first URI record, expanded", () => {
    expect(matchKey([textRecord("t"), uriRecord("https://topps.drey.dev/c/1"), uriRecord("https://x")])).toBe(
      "https://topps.drey.dev/c/1",
    );
  });
  it("falls back to the first Text record without its language", () => {
    expect(matchKey([mimeRecord("a/b", "x"), textRecord("CARD-0042", "en-US"), textRecord("other")])).toBe("CARD-0042");
  });
  it("returns null without URI or Text", () => {
    expect(matchKey([mimeRecord("a/b", "x")])).toBeNull();
    expect(matchKey([])).toBeNull();
  });
});

describe("describeRecord", () => {
  it("decodes common types", () => {
    expect(describeRecord(uriRecord("tel:+1555"))).toBe("tel:+1555");
    expect(describeRecord(textRecord("hi", "fr"))).toBe("[fr, utf-8] hi");
    expect(describeRecord(mimeRecord("application/json", "{}"))).toBe("{}");
    expect(describeRecord(mimeRecord("image/png", new Uint8Array(3)))).toBe("3 bytes");
  });
});
