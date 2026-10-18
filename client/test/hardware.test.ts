// Real-hardware check. Needs the bridge running and a writable NTAG21x on a reader.
// It overwrites the tag, then restores the original NDEF message.
//
//   BRIDGE_HW=1 npx vitest run test/hardware.test.ts
//
// BRIDGE_READER=<exact name> picks a reader; default: the first reader with a card.

import { describe, expect, it } from "vitest";
import { BridgeClient } from "../src/client.js";
import { encodeNdef, mimeRecord, textRecord, uriRecord } from "../src/ndef.js";
import { eraseNdef, isType2, maxMessageBytes, readCC, readNdef, writeNdef } from "../src/ntag.js";

const env = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env ?? {};

describe.skipIf(!env.BRIDGE_HW)("hardware", () => {
  it("reads, writes, fills, erases and restores a real tag", { timeout: 120_000 }, async () => {
    const client = new BridgeClient();
    client.connect();
    const card = await new Promise<{ reader: string; atr: string; uid: string | null }>((resolve) => {
      const check = () => {
        const r = client.readers.find((x) => x.card && (!env.BRIDGE_READER || x.name === env.BRIDGE_READER));
        if (r?.card) resolve({ reader: r.name, atr: r.card.atr, uid: r.card.uid });
      };
      client.on("readers", check);
      check();
    });
    console.log(`bridge ${client.version}; reader "${card.reader}", UID ${card.uid}, ATR ${card.atr}`);
    expect(isType2(card.atr)).toBe(true);
    expect(card.uid).toMatch(/^([0-9A-F]{8}|[0-9A-F]{14}|[0-9A-F]{16}|[0-9A-F]{20})$/);

    const cc = await readCC(client, card.reader, card.atr);
    console.log(`CC ${cc.raw}: ${cc.model}, ${cc.dataBytes} bytes`);
    const original = await readNdef(client, card.reader, card.atr);
    console.log(`original: ${original.records.length} record(s), ${original.message.length} bytes`);

    try {
      const records = [uriRecord("https://example.com/hw-test"), textRecord("Grüße", "de", "utf-16"), mimeRecord("application/json", '{"hw":1}')];
      let t = performance.now();
      const w = await writeNdef(client, card.reader, card.atr, records);
      console.log(`multi-record write: ${w.pagesWritten} pages in ${Math.round(performance.now() - t)} ms, transparent=${w.usedTransparentSession}`);
      expect((await readNdef(client, card.reader, card.atr)).records).toEqual(records);

      const max = maxMessageBytes(cc.dataBytes - original.tlvOffset);
      const longForm = max - 9 >= 256;
      const overhead = (longForm ? 6 : 3) + 3;
      const full = [mimeRecord("a/b", new Uint8Array(max - overhead).map((_, i) => (i * 7) & 0xff))];
      expect(encodeNdef(full).length).toBe(max);
      t = performance.now();
      const f = await writeNdef(client, card.reader, card.atr, full);
      console.log(`full-capacity write (${max} bytes): ${f.pagesWritten} pages in ${Math.round(performance.now() - t)} ms`);
      t = performance.now();
      expect((await readNdef(client, card.reader, card.atr)).records).toEqual(full);
      console.log(`full read: ${Math.round(performance.now() - t)} ms`);

      await eraseNdef(client, card.reader, card.atr);
      expect((await readNdef(client, card.reader, card.atr)).records).toEqual([]);
    } finally {
      await writeNdef(client, card.reader, card.atr, original.message);
      console.log("original content restored");
      client.close();
    }
  });
});
