# NFC Browser Bridge

Desktop Chrome cannot talk to USB smart-card (CCID) NFC readers directly: WebUSB
blocks the smart-card class and the Web Smart Card API only ships on ChromeOS.
NFC Browser Bridge is a small menu-bar (macOS) / system-tray (Windows) app that
exposes the operating system's smart-card service (PC/SC) to web pages over a
local WebSocket.

- It reports readers and cards (UID, ATR) and passes raw APDUs through.
- All NTAG/NDEF logic lives in the TypeScript client in [`client/`](client/).
- One `.app` on macOS, one `.exe` on Windows; no runtimes to install.

Supported readers: **ACS ACR1552U** and **ACS ACR1255U-J1** over USB. Other
PC/SC contactless readers that support the PC/SC Part 3 storage-card commands
(`FF CA`, `FF B0`, `FF D6`) are likely to work.

## How a page connects

```ts
import { BridgeClient } from "./client/src/client";
import { readNdef, matchKey } from "./client/src/ntag";

const bridge = new BridgeClient(); // ws://127.0.0.1:38241/v1
bridge.on("card_present", async ({ reader, uid, atr }) => {
  const { records } = await readNdef(bridge, reader, atr);
  console.log(uid, matchKey(records));
});
bridge.connect();
```

The wire protocol is documented in [docs/PROTOCOL.md](docs/PROTOCOL.md). The
test site in [`web/`](web/) exercises everything: live readers, NDEF read and
write (URI, Text, MIME, several records per message), erase, and a raw APDU
console.

## Install

### macOS

1. Download [NFC-Browser-Bridge-macos.dmg](https://github.com/andreykats/nfc-browser-bridge/releases/latest/download/NFC-Browser-Bridge-macos.dmg)
   from the latest release and drag
   **NFC Browser Bridge** to **Applications**. Start it from Applications, not
   from the disk image, or "Start at login" cannot turn on.
2. The build is not signed by Apple, so the first launch is blocked. Open
   **System Settings → Privacy & Security**, scroll to the message about
   NFC Browser Bridge and click **Open Anyway**. This repeats after every update.
3. A card icon appears in the menu bar. **Start at login** turns on by itself
   the first time the app runs from Applications; it can be turned off in the
   menu and stays off.

### Windows

1. Download [NFC-Browser-Bridge-windows-setup.exe](https://github.com/andreykats/nfc-browser-bridge/releases/latest/download/NFC-Browser-Bridge-windows-setup.exe)
   from the latest release and run it. No administrator rights are needed.
2. The installer is not signed, so SmartScreen warns about it: choose
   **More info → Run anyway**.
3. "Start at login" is selected by default in the installer and can be changed
   later from the tray menu.
4. Windows hides new tray icons in the overflow area. Drag the card icon onto
   the taskbar (or enable it under Settings → Personalization → Taskbar → Other
   system tray icons) so it stays visible.

## Reader setup

- **ACR1552U:** the factory "CCID only" mode is what the bridge needs.
  Whether "HID Keyboard + CCID Reader" mode also works alongside the bridge is
  still to be confirmed on Windows; see [docs/HARDWARE.md](docs/HARDWARE.md).
- **ACR1255U-J1:** connect it with the USB cable. On macOS it works with the
  built-in driver.

## Chrome permission ("Apps on device")

Chrome asks each website once for permission to connect to apps on this device
(the `loopback-network` permission of
[Local Network Access](https://wicg.github.io/local-network-access/)). Choose
**Allow**. The grant is kept per Chrome profile; a fresh `--user-data-dir`
starts without it. If you blocked it: site settings (icon left of the address
bar) → **Apps on device** → Allow, then reload.

Managed machines can skip the prompt with the Chrome policy
`LoopbackNetworkAllowedForUrls` (Chrome 146+; `LocalNetworkAccessAllowedForUrls`
on older versions):

- **Windows** (registry, per machine):
  ```
  HKLM\SOFTWARE\Policies\Google\Chrome\LoopbackNetworkAllowedForUrls
    1 = "https://topps.drey.dev"
  ```
- **macOS:** a configuration profile for the `com.google.Chrome` domain with
  `LoopbackNetworkAllowedForUrls` set to an array containing
  `https://topps.drey.dev`.

Pages on `http://localhost` never see the prompt, so test the real flow on the
published site.

## Security note

While the bridge runs, **any website you open can use the NFC reader**: list
readers, read card UIDs, and read or write cards. The bridge refuses writes to
NTAG pages 0–3 (UID, lock bytes, capability container) unless a request sets
`unsafe: true`, which prevents the most damaging mistakes, but it is not a
security boundary. Quit the bridge when you do not need it. The tray menu lists
the websites currently connected.

## Troubleshooting

- **Logs:** tray menu → **Open log folder**.
  macOS: `~/Library/Logs/NFCBrowserBridge/`;
  Windows: `%LOCALAPPDATA%\NFCBrowserBridge\logs\`.
  Daily files, last 7 kept. Start the app with `RUST_LOG=debug` to log APDUs.
- **"Port 38241 in use by another app":** another program listens on the
  bridge's port. The bridge retries every 5 seconds.
- **"Smart card service unavailable" (macOS) / "No reader connected" (Windows):**
  Windows stops its smart-card service when the last reader is unplugged; plug
  the reader in and the bridge reconnects automatically.
- **Windows "installing device driver" pop-ups or `sharing_violation` errors:**
  turn off Smart Card Plug and Play by setting the DWORD
  `HKLM\SOFTWARE\Policies\Microsoft\Windows\ScPnP\EnableScPnP` to `0`.
- **macOS driver:** the built-in CCID driver handled the ACR1255U-J1 in testing,
  so ACS's macOS driver is not needed.

## Development

```sh
cargo test --workspace                       # Rust (uses FakePcsc, no hardware)
cargo run -p bridge-app                      # tray app from source
cargo run -p bridge-core --example serve     # bridge without the tray
(cd client && npm ci && npm test)            # client library
(cd web && npm ci && npm run dev)            # test site at http://localhost:5173/nfc-browser-bridge/
```

Hardware tools and the release checklist are in
[docs/HARDWARE.md](docs/HARDWARE.md). Releases are built by
[`.github/workflows/release.yml`](.github/workflows/release.yml) when a `v*`
tag is pushed; the test site is published to GitHub Pages by
[`pages.yml`](.github/workflows/pages.yml).

## License

MIT
