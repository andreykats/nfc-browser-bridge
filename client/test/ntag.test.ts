import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { BridgeClient } from "../src/client.js";
import { FakeBridge, FakeTag, type NtagKind } from "../src/fakeBridge.js";
import { encodeNdef, mimeRecord, parseNdef, textRecord, uriRecord, type NdefRecord } from "../src/ndef.js";
import {
  NtagError,
  eraseNdef,
  fromHex,
  isType2,
  maxMessageBytes,
  planWrite,
  readCC,
  readNdef,
  toHex,
  writeNdef,
} from "../src/ntag.js";
import { locateNdef } from "../src/tlv.js";

const R = "ACS ACR1552 1S CL Reader PICC 0";
let fake: FakeBridge;
let client: BridgeClient;

async function connected(): Promise<void> {
  client.connect();
  await new Promise<void>((resolve) => {
    if (client.state === "open") resolve();
    const off = client.on("state", (s) => s === "open" && (off(), resolve()));
  });
}

function place(tag: FakeTag): FakeTag {
  fake.placeCard(R, tag);
  return tag;
}

beforeEach(async () => {
  fake = new FakeBridge();
  fake.addReader(R);
  client = new BridgeClient({ WebSocket: fake.WebSocket });
  await connected();
});

afterEach(() => client.close());

describe("isType2", () => {
  it("accepts PC/SC part 3 card name 00 03 only", () => {
    expect(isType2("3B8F8001804F0CA0000003060300030000000068")).toBe(true);
    expect(isType2("3B8F8001804F0CA000000306030001000000006A")).toBe(false); // MIFARE Classic 1K
    expect(isType2("3B8180018080")).toBe(false); // ISO 14443-4 (DESFire-style)
    expect(isType2("zz")).toBe(false);
  });
});

describe("CC", () => {
  it.each([
    ["NTAG213", 144, 39],
    ["NTAG215", 496, 127],
    ["NTAG216", 872, 221],
  ] as const)("detects %s", async (kind, bytes, lastPage) => {
    const tag = place(FakeTag.ntag(kind));
    const cc = await readCC(client, R, tag.atr);
    expect(cc).toMatchObject({ model: kind, dataBytes: bytes, firstPage: 4, lastPage, writable: true, readable: true });
  });

  it("rejects an unformatted card", async () => {
    const tag = place(FakeTag.ntag("NTAG213"));
    tag.memory.set([0, 0, 0, 0], 12);
    await expect(readNdef(client, R, tag.atr)).rejects.toMatchObject({ code: "not_ndef" });
  });
});

describe("readNdef", () => {
  it("reads a factory-blank tag as empty", async () => {
    const tag = place(FakeTag.ntag("NTAG215"));
    const n = await readNdef(client, R, tag.atr);
    expect(n.records).toEqual([]);
    expect(n.tlvOffset).toBe(0);
  });

  it("reads only the pages it needs, in at most two batches", async () => {
    const records = [uriRecord("https://topps.drey.dev/c/1"), textRecord("x".repeat(100))];
    const tag = place(FakeTag.ntag("NTAG216").withNdef(records));
    const requests: number[] = [];
    const orig = client.transmit.bind(client);
    client.transmit = (reader, apdus, o) => (requests.push(apdus.length), orig(reader, apdus, o));
    const n = await readNdef(client, R, tag.atr);
    expect(n.records).toEqual(records);
    expect(requests.length).toBe(2);
    const tlvBytes = 2 + encodeNdef(records).length;
    // First batch: CC + 16 bytes; second: the rest, rounded up to pages.
    expect(requests[1]).toBe(Math.ceil((Math.ceil(tlvBytes / 4) * 4 - 16) / 16));
  });

  it("refuses non-Type-2 cards without sending anything", async () => {
    const tag = place(FakeTag.classic1k());
    await expect(readNdef(client, R, tag.atr)).rejects.toMatchObject({ code: "not_type2" });
    await expect(writeNdef(client, R, tag.atr, [textRecord("x")])).rejects.toBeInstanceOf(NtagError);
    expect(fake.apduLog).toEqual([]);
  });
});

describe("writeNdef", () => {
  it.each(["NTAG213", "NTAG215", "NTAG216"] as NtagKind[])("round-trips a multi-record message on %s", async (kind) => {
    const tag = place(FakeTag.ntag(kind));
    const records: NdefRecord[] = [
      uriRecord("https://topps.drey.dev/card/123"),
      textRecord("Topps card", "en"),
      mimeRecord("application/json", '{"id":123}'),
    ];
    const res = await writeNdef(client, R, tag.atr, records);
    expect(res.usedTransparentSession).toBe(false);
    const n = await readNdef(client, R, tag.atr);
    expect(n.records).toEqual(records);
  });

  it("fills an NTAG216 to capacity and never writes outside user memory", async () => {
    const tag = place(FakeTag.ntag("NTAG216"));
    const before = tag.memory.slice();
    const max = maxMessageBytes(872);
    // A MIME record whose encoded message is exactly `max` bytes (3-byte header + type + payload, long form: 6).
    const type = "a/b";
    const payload = new Uint8Array(max - 6 - type.length).map((_, i) => i & 0xff);
    const records = [mimeRecord(type, payload)];
    expect(encodeNdef(records).length).toBe(max);
    await writeNdef(client, R, tag.atr, records);
    expect((await readNdef(client, R, tag.atr)).records).toEqual(records);
    // Pages 0-3 and everything past page 221 untouched.
    expect(tag.memory.subarray(0, 16)).toEqual(before.subarray(0, 16));
    expect(tag.memory.subarray(222 * 4)).toEqual(before.subarray(222 * 4));
    // One byte more does not fit.
    const tooBig = [mimeRecord(type, new Uint8Array(payload.length + 1))];
    await expect(writeNdef(client, R, tag.atr, tooBig)).rejects.toMatchObject({ code: "capacity", needed: 873, available: 872 });
  });

  it("rejects messages too large for an NTAG213", async () => {
    const tag = place(FakeTag.ntag("NTAG213"));
    await expect(writeNdef(client, R, tag.atr, [textRecord("x".repeat(200))])).rejects.toMatchObject({ code: "capacity" });
    expect((await readNdef(client, R, tag.atr)).records).toEqual([]);
  });

  it("keeps a lock control TLV in front of the NDEF TLV", async () => {
    const tag = place(FakeTag.ntag("NTAG215"));
    tag.memory.set(fromHex("0103A00C340300FE"), 16);
    await writeNdef(client, R, tag.atr, [uriRecord("https://a.b")]);
    expect(toHex(tag.memory.subarray(16, 21))).toBe("0103A00C34");
    const n = await readNdef(client, R, tag.atr);
    expect(n.tlvOffset).toBe(5);
    expect(n.records).toEqual([uriRecord("https://a.b")]);
  });

  it("erases to an empty NDEF TLV", async () => {
    const tag = place(FakeTag.ntag("NTAG213").withNdef([textRecord("old")]));
    await eraseNdef(client, R, tag.atr);
    expect(toHex(tag.memory.subarray(16, 19))).toBe("0300FE");
    expect((await readNdef(client, R, tag.atr)).records).toEqual([]);
  });

  it("refuses read-only tags", async () => {
    const tag = place(FakeTag.ntag("NTAG213"));
    tag.memory[15] = 0x0f;
    await expect(writeNdef(client, R, tag.atr, [textRecord("x")])).rejects.toMatchObject({ code: "read_only" });
  });

  it("falls back to the ACR1552 transparent session when FF D6 fails", async () => {
    const tag = place(FakeTag.ntag("NTAG215"));
    fake.failNextWrites(1);
    const records = [uriRecord("https://topps.drey.dev/x"), textRecord("fallback")];
    const res = await writeNdef(client, R, tag.atr, records);
    expect(res.usedTransparentSession).toBe(true);
    expect(fake.apduLog.some((a) => a.startsWith("FFC2000102") || a.startsWith("FFC20001"))).toBe(true);
    expect((await readNdef(client, R, tag.atr)).records).toEqual(records);
  });

  it("gives up with the original error when the fallback also fails", async () => {
    const tag = place(FakeTag.ntag("NTAG215"));
    fake.failNextWrites(1);
    // Break the transparent session: the reader answers 6A81 to FF C2.
    const orig = client.transmit.bind(client);
    client.transmit = (reader, apdus, o) =>
      apdus[0] === "FFC20000028100"
        ? Promise.resolve([{ data: "", sw: "6A81" }])
        : orig(reader, apdus, o);
    await expect(writeNdef(client, R, tag.atr, [textRecord("x")])).rejects.toMatchObject({ code: "apdu_failed", sw: "6300" });
  });

  it("leaves an empty or valid tag when the card is lifted mid-write", async () => {
    const oldRecords = [textRecord("old content that is fairly long ".repeat(3))];
    const newRecords = [uriRecord("https://topps.drey.dev/new"), textRecord("new ".repeat(60))];
    const oldMsg = toHex(encodeNdef(oldRecords));
    const newMsg = toHex(encodeNdef(newRecords));
    let sawEmpty = false;
    let sawOld = false;
    for (let n = 2; n < 40; n++) {
      const tag = FakeTag.ntag("NTAG215").withNdef(oldRecords);
      place(tag);
      fake.removeCardAfterApdus(n);
      await expect(writeNdef(client, R, tag.atr, newRecords)).rejects.toMatchObject({ code: "card_removed" });
      // Inspect the image directly: the first NDEF TLV must parse.
      const loc = locateNdef(tag.memory.subarray(16), 496);
      expect(loc.kind).toBe("found");
      if (loc.kind !== "found") return;
      const msg = tag.memory.subarray(16 + loc.valueOffset, 16 + loc.valueOffset + loc.length);
      parseNdef(msg); // does not throw
      const hex = toHex(msg);
      expect([oldMsg, newMsg, ""]).toContain(hex);
      sawEmpty ||= hex === "";
      sawOld ||= hex === oldMsg;
    }
    expect(sawEmpty && sawOld).toBe(true);
  });
});

describe("planWrite", () => {
  it("writes the empty header first and the real header last", () => {
    const msg = new Uint8Array(300).fill(0xaa);
    const writes = planWrite(new Uint8Array(496), 2, msg, 496);
    // Header 03 FF 01 2C at offset 2 spans pages 4 and 5.
    expect(writes[0]).toEqual({ page: 4, data: fromHex("00000300") });
    expect(writes[1]).toEqual({ page: 5, data: fromHex("FE000000") });
    const last2 = writes.slice(-2);
    expect(last2[0]).toEqual({ page: 5, data: fromHex("012CAAAA") });
    expect(last2[1]).toEqual({ page: 4, data: fromHex("000003FF") });
  });
});
