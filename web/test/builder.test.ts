import { describe, expect, it } from "vitest";
import { entryToRecord, parseApduLines, usage } from "../src/builder.js";
import { decodeText, decodeUri, parseNdef, encodeNdef } from "../../client/src/ndef.js";

describe("entryToRecord", () => {
  it("builds URI, Text and MIME records", () => {
    const recs = [
      entryToRecord({ kind: "uri", uri: "https://topps.drey.dev/c/9" }),
      entryToRecord({ kind: "text", text: "hi", lang: "en-US", encoding: "utf-16" }),
      entryToRecord({ kind: "mime", mimeType: "application/json", payload: "{}" }),
    ];
    const back = parseNdef(encodeNdef(recs));
    expect(decodeUri(back[0])).toBe("https://topps.drey.dev/c/9");
    expect(decodeText(back[1])).toEqual({ text: "hi", lang: "en-US", encoding: "utf-16" });
    expect(new TextDecoder().decode(back[2].payload)).toBe("{}");
  });

  it("validates inputs", () => {
    expect(() => entryToRecord({ kind: "uri", uri: "" })).toThrow();
    expect(() => entryToRecord({ kind: "text", text: "x", lang: "e n", encoding: "utf-8" })).toThrow();
    expect(() => entryToRecord({ kind: "mime", mimeType: "nope", payload: "" })).toThrow();
  });
});

describe("usage", () => {
  it("counts TLV bytes against the data area", () => {
    const r = [entryToRecord({ kind: "text", text: "x".repeat(130), lang: "en", encoding: "utf-8" })];
    // record: 4 header + 1 status + 2 lang + 130 = 137; TLV +2 = 139
    expect(usage(r, 144)).toEqual({ used: 139, capacity: 144, fits: true, maxMessage: 142 });
    expect(usage(r, 144, 10).fits).toBe(false);
  });
});

describe("parseApduLines", () => {
  it("accepts spaced hex and comments", () => {
    expect(parseApduLines("FF CA 00 00 00  # uid\n\nff:b0:00:04:10\n")).toEqual(["FFCA000000", "FFB0000410"]);
    expect(() => parseApduLines("FFC")).toThrow(/Line 1/);
  });
});
