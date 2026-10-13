import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BridgeClient, BridgeError, type ConnectionState } from "../src/client.js";
import { FakeBridge, FakeTag } from "../src/fakeBridge.js";
import type { CardPresentEvent } from "../src/protocol.js";

const R = "ACS ACR1255U-J1";

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

const tick = (ms = 0) => vi.advanceTimersByTimeAsync(ms);

function setup() {
  const fake = new FakeBridge();
  fake.addReader(R);
  const client = new BridgeClient({ WebSocket: fake.WebSocket });
  const states: ConnectionState[] = [];
  const taps: CardPresentEvent[] = [];
  client.on("state", (s) => states.push(s));
  client.on("card_present", (e) => taps.push(e));
  return { fake, client, states, taps };
}

describe("BridgeClient", () => {
  it("does not report cards from hello as taps", async () => {
    const { fake, client, taps } = setup();
    const tag = FakeTag.ntag("NTAG213");
    fake.placeCard(R, tag);
    client.connect();
    await tick();
    expect(client.state).toBe("open");
    expect(client.readers[0].card?.uid).toBe(tag.uidHex);
    expect(taps).toEqual([]);

    fake.removeCard(R);
    fake.placeCard(R, tag);
    await tick();
    expect(taps.map((t) => t.uid)).toEqual([tag.uidHex]);
    client.close();
  });

  it("reconnects with backoff and does not replay a present card", async () => {
    const { fake, client, states, taps } = setup();
    fake.placeCard(R, FakeTag.ntag("NTAG215"));
    client.connect();
    await tick();
    expect(client.version).toBe("0.0.0-fake");

    fake.stop();
    await tick();
    expect(client.state).toBe("connecting");
    expect(client.readers).toEqual([]);

    // Bridge stays down for a while: attempts at 0.5, 1, 2, 4 s… capped at 10 s.
    await tick(500 + 1000 + 2000 + 4000 + 8000);
    expect(client.state).toBe("connecting");
    fake.start();
    await tick(10_000);
    expect(client.state).toBe("open");
    expect(client.readers[0].card).not.toBeNull();
    expect(taps).toEqual([]);
    expect(states.filter((s) => s === "open").length).toBe(2);

    // Backoff resets after a successful hello.
    fake.stop();
    await tick();
    fake.start();
    await tick(500);
    expect(client.state).toBe("open");
    client.close();
    expect(client.state).toBe("closed");
  });

  it("times out requests and fails pending ones on disconnect", async () => {
    const { fake, client } = setup();
    client.connect();
    await tick();
    fake.silent = true;
    const p = client.ping();
    const caught = p.catch((e) => e);
    await tick(5000);
    expect(await caught).toMatchObject({ code: "timeout" });

    const q = client.transmit(R, ["FFCA000000"]).catch((e) => e);
    fake.stop();
    await tick();
    const err = await q;
    expect(err).toBeInstanceOf(BridgeError);
    expect(err.code).toBe("disconnected");
    await expect(client.ping()).rejects.toMatchObject({ code: "disconnected" });
    client.close();
  });

  it("scales the transmit timeout with batch size", async () => {
    const { fake, client } = setup();
    fake.placeCard(R, FakeTag.ntag("NTAG216"));
    client.connect();
    await tick();
    fake.silent = true;
    const q = client.transmit(R, Array(200).fill("FFB0000410")).catch((e) => e);
    await tick(5000 + 200 * 25 - 1);
    let settled = false;
    q.then(() => (settled = true));
    await tick(0);
    expect(settled).toBe(false);
    await tick(1);
    expect(await q).toMatchObject({ code: "timeout" });
    client.close();
  });

  it("maps bridge errors to BridgeError", async () => {
    const { fake, client } = setup();
    client.connect();
    await tick();
    const e = await client.transmit(R, ["FFCA000000"]).catch((x) => x);
    expect(e).toMatchObject({ code: "no_card", pcsc: "0x8010000C" });
    fake.placeCard(R, FakeTag.ntag("NTAG213"));
    await tick();
    const w = await client.transmit(R, ["FFD6000304E1100000"]).catch((x) => x);
    expect(w).toMatchObject({ code: "blocked_write" });
    expect(await client.transmit(R, ["FFCA000000"])).toEqual([{ data: "04699E22C01C90", sw: "9000" }]);
    expect(await client.reset(R)).toBe("3B8F8001804F0CA0000003060300030000000068");
    client.close();
  });

  it("rejects a bridge speaking another protocol and stops reconnecting", async () => {
    const { fake, client } = setup();
    const errors: BridgeError[] = [];
    client.on("error", (e) => errors.push(e));
    fake.connected = (s) =>
      s.deliver({ event: "hello", protocol: 2, app: "topps-nfc-bridge", version: "2.0.0", service: "ready", readers: [] });
    client.connect();
    await tick();
    expect(errors.map((e) => e.code)).toEqual(["protocol_mismatch"]);
    expect(client.state).toBe("closed");
    await tick(20_000);
    expect(client.state).toBe("closed");
  });

  it("shares one bridge between two clients", async () => {
    const { fake, client } = setup();
    const other = new BridgeClient({ WebSocket: fake.WebSocket });
    const seen: string[] = [];
    client.on("card_present", () => seen.push("a"));
    other.on("card_present", () => seen.push("b"));
    client.connect();
    other.connect();
    await tick();
    expect(fake.connections).toBe(2);
    fake.placeCard(R, FakeTag.ntag("NTAG213"));
    await tick();
    expect(seen.sort()).toEqual(["a", "b"]);
    client.close();
    other.close();
  });
});
