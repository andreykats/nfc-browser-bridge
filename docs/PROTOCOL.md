# NFC Browser Bridge protocol, version 1

This document is frozen for protocol version 1. Additive, backwards-compatible
changes (new optional fields, new events that clients may ignore) are allowed;
anything else bumps the version and the URL path.

The TypeScript definitions in [`client/src/protocol.ts`](../client/src/protocol.ts)
mirror this document exactly.

## Connection

- URL: `ws://127.0.0.1:38241/v1`.
  - Clients always connect to `127.0.0.1`, never `localhost`. `localhost` may
    resolve to `::1`, and the bridge only listens on IPv4 loopback.
- The bridge binds `127.0.0.1:38241` only.
- Any `Origin` is accepted. The origin is recorded and shown in the tray menu.
- The `Host` header must be exactly `127.0.0.1:38241` or `localhost:38241`.
  Anything else is rejected with HTTP 403. This guards against DNS rebinding.
- Any path other than `/v1` is rejected with HTTP 404.
- All messages are UTF-8 JSON text frames. Binary frames are rejected with
  `bad_request`.
- The server sends a WebSocket ping every 30 s and closes connections that
  have not sent anything (including pongs) for 90 s.

Chrome gates loopback connections from public pages behind the
`loopback-network` permission ("Apps on device"). See the README.

### Limits

| Limit                          | Value                                         |
| ------------------------------ | --------------------------------------------- |
| Message size                   | 1 MiB                                         |
| APDUs per `transmit` batch     | 600                                           |
| Bytes per APDU                 | 4 to 261                                      |
| In-flight requests per socket  | 32 (more get `busy`)                          |

APDUs and response data are hex strings. The server accepts upper or lower
case and no separators; it always sends uppercase.

## Data types

```ts
type Reader = {
  name: string;                      // exact PC/SC reader name; the reader's identity
  kind: "picc" | "sam" | "other";    // contactless slot, SAM slot, or unknown
  card: { uid: string | null; atr: string } | null;
};
```

- `uid` is uppercase hex, no separators: 8, 14, 16 or 20 characters for 4-,
  7-, 8- or 10-byte UIDs. It comes from `FF CA 00 00 00`. It is `null` when the
  card did not answer that command (for example a contact card).
- `atr` is uppercase hex.
- Readers are addressed by `name`, never by index.

`service` is `"ready"` when the OS smart-card service is reachable and
`"unavailable"` otherwise (for example Windows stops the service when the last
reader is unplugged). When it is `"unavailable"`, `readers` is empty.

Where the bridge can see USB (macOS), `hello`, `readers` and `list_readers`
also carry:

```ts
usbReaders: { name: string }[];                // smart-card readers on USB, listed or not
recovery: "idle" | "recovering" | "failed";    // see "Reader recovery"
```

A reader in `usbReaders` but not in `readers` is plugged in but not working.
Both fields are absent on other platforms; clients must not rely on them.

## Server events

Events have an `event` field and no `id`.

### `hello`

Sent once, immediately after the connection opens.

```json
{"event":"hello","protocol":1,"app":"nfc-browser-bridge","version":"0.1.0","service":"ready",
 "readers":[{"name":"ACS ACR1552 1S CL Reader PICC 0","kind":"picc","card":null}]}
```

A card that is already listed in `hello` was on the reader before this
connection opened. Clients must **not** treat it as a new tap.

### `readers`

The full reader list. Sent whenever anything in it changes: a reader appears
or disappears, a card arrives or leaves, or the service status changes.

```json
{"event":"readers","service":"ready","readers":[...]}
```

Where the bridge can see USB, `readers` is also sent when `usbReaders` or
`recovery` changes.

### `recovery`

A step the bridge took to bring a stuck reader back (see "Reader recovery").
Meant for logs; the outcome shows in `recovery` and the reader list.

```json
{"event":"recovery","action":"usb_reset","reason":"the reader is on USB but the smart card service does not list it (attempt 1 of 2)","readers":["ACS ACR1255U-J1"]}
```

`action` is `reopen` (reconnected to the smart-card service), `usb_reset`
(reset readers over USB), `failed` (gave up) or `recovered`. `readers` names
the readers concerned (USB names for `usb_reset`).

### `card_present`

A card was placed on a reader and its UID has been read. Sent before the
`readers` event that reflects it.

```json
{"event":"card_present","reader":"ACS ACR1255U-J1","uid":"04699E22C01C90","atr":"3B8F8001804F0CA0000003060300030000000068"}
```

### `card_removed`

```json
{"event":"card_removed","reader":"ACS ACR1255U-J1"}
```

`card_removed` is only sent for cards that were announced (by `card_present`
or in `hello`/`readers`). A card is announced at most once per placement.

When the bridge resets a reader (see "Reader recovery"), the reader leaves and
comes back. A card still lying on it gets `card_removed`, then shows up again
in `readers` **without** a `card_present`: it was never lifted.

## Requests

Requests have an `id` (a string or a number, echoed back unchanged) and a
`type`. A request may carry `"protocol": 1`; any other value is answered
with `protocol_mismatch`.

### `ping`

```json
{"id":1,"type":"ping"}
```

Result: `{}`.

### `list_readers`

```json
{"id":2,"type":"list_readers"}
```

Result: `{"service":"ready","readers":[Reader]}`, plus `usbReaders` and
`recovery` where the bridge can see USB.

### `transmit`

```json
{"id":3,"type":"transmit","reader":"ACS ACR1255U-J1","apdus":["FFCA000000"],"stopOnError":true,"unsafe":false}
```

- `stopOnError` (default `true`): stop at the first response whose status
  word is not `9000`. The responses so far, including the failing one, are
  returned with `ok: true`.
- `unsafe` (default `false`): see the write guard below.
- The whole batch runs on one connection inside one PC/SC transaction
  (`SCardBeginTransaction` / `SCardEndTransaction`), so no other application
  can interleave commands. Batches from all clients run one at a time.

Result:

```json
{"responses":[{"data":"04699E22C01C90","sw":"9000"}]}
```

`data` is the response without the status word; `sw` is the last two bytes.

If a PC/SC call fails part-way, the request fails with `transmit_failed`,
`card_removed` or `no_card`, and `error.completed` says how many APDUs
completed before the failure. The card is then reset (disconnected with
`SCARD_RESET_CARD`).

### `reset`

```json
{"id":4,"type":"reset","reader":"ACS ACR1255U-J1"}
```

Reconnects to the card with `SCARD_RESET_CARD`. Use it to recover a reader
that is stuck after a failed command. Result: `{"atr":"3B8F..."}`.

### `reset_readers`

```json
{"id":5,"type":"reset_readers"}
```

Resets every smart-card reader on USB, the software equivalent of unplugging
it and plugging it back in (the reader keeps its power). For a staff "reset
reader" button. Result: `{"reset":["ACS ACR1255U-J1"]}`, the readers that were
reset. Fails with `unsupported` where the bridge cannot see USB.

## Responses

Success:

```json
{"id":3,"ok":true,"result":{...}}
```

Failure:

```json
{"id":3,"ok":false,"error":{"code":"no_card","message":"No card on reader","pcsc":"0x8010000C"}}
```

- `pcsc` is present when the error came from a PC/SC call: the raw status
  code as `0x` + 8 uppercase hex digits.
- `completed` is present on `transmit` failures (see above).
- If a message cannot be parsed well enough to find its `id`, the response has
  `"id": null`.

### Error codes

| Code                | Meaning                                                                    |
| ------------------- | -------------------------------------------------------------------------- |
| `bad_request`       | Malformed JSON, unknown `type`, missing or invalid fields, bad hex, limits |
| `protocol_mismatch` | Request `protocol` is not 1 (the client also uses it for `hello.protocol`) |
| `no_such_reader`    | No reader with that name                                                   |
| `no_card`           | No card on the reader                                                      |
| `card_removed`      | The card left the field during the request                                 |
| `sharing_violation` | Another application holds the card exclusively (after 3 retries)           |
| `transmit_failed`   | Any other PC/SC failure while talking to the card                          |
| `blocked_write`     | Write to pages 0–3 without `unsafe: true`                                  |
| `pcsc_unavailable`  | The smart-card service is not running                                      |
| `busy`              | Too many in-flight requests on this socket, or the bridge queue is full    |
| `unsupported`       | The platform cannot do this (`reset_readers` without USB access)           |

## Write guard

Any website can connect to the bridge, so writes to NFC Forum Type 2 pages
0–3 (UID, static lock bytes, capability container) are refused with
`blocked_write` unless the request has `"unsafe": true`. The guard checks:

- `FF D6 P1 P2 Lc …` (UPDATE BINARY): blocked when any of the pages
  `P1P2 … P1P2 + ceil(Lc / 4) - 1` is below 4.
- `FF C2 00 01 Lc 95 n A2 page …` and `… A0 page …` (ACS transparent session
  WRITE / COMPATIBILITY WRITE): blocked when `page` is below 4.
- `FF 00 00 00 Lc D4 40 tg A2|A0 page …` and `FF 00 00 00 Lc D4 42 A2|A0 page …`
  (PN532 InDataExchange / InCommunicateThru pass-through, ACR122-style
  readers): blocked when `page` is below 4.

This is a cheap safety net, not a security boundary.

## Reader recovery

Where the bridge can see USB (macOS), it watches for two kinds of trouble:

- **Missing:** a smart-card reader is on USB, but the smart-card service lists
  no usable reader (the kiosk would say "plug in the reader" while it is
  plugged in).
- **Unresponsive:** a card held on a reader stops answering. The bridge checks
  held cards every 60 s; a card that fails is reset (powered off and on) first.

A missing reader gets 5 s to show up (a replug takes about 1 s). Then the
bridge reconnects to the smart-card service, and after 5 more seconds resets
the reader over USB. An unresponsive card goes straight to the USB reset. If
the reader still does not work 10 s after a reset, it is reset once more, and
after that `recovery` becomes `"failed"`: the reader needs unplugging and
plugging back in. Replugging it, or the reader coming back by itself, returns
`recovery` to `"idle"`.

`recovery` is `"recovering"` while the bridge reconnects or waits after a
reset. Each step is reported with a `recovery` event.

Kiosks with several readers: only "no usable reader at all" counts as missing.
