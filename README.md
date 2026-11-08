# NFC Browser Bridge

NFC Browser Bridge is a small app that lets websites in Google Chrome use an NFC
reader plugged into your computer. It sits in the menu bar on a Mac or the
system tray on Windows, starts when you log in, and runs quietly in the
background.

Chrome can't talk to USB NFC readers on its own. The bridge connects the two:
when you tap an NFC tag or card on the reader, the website you have open sees it
straight away.

## What it's used for

- **Video kiosks:** the [Kiosk](https://github.com/andreykats/kiosk-video-nfc)
  app plays a video when someone taps a card on the reader. With the bridge,
  taps work even when the kiosk window isn't in front, and the kiosk can read
  the card ID stored on the tag.
- **Reading and writing NFC tags in the browser:** the
  [test site](https://andreykats.github.io/nfc-browser-bridge/) shows the
  readers and tags the bridge sees, and can read, write and erase tags (web
  links, text, and other data).
- **Your own web pages:** any website can use the reader through the bridge
  while it runs (see [Security](#security)).

## Readers

| Reader | Mac | Windows |
| --- | --- | --- |
| **ACS ACR1255U-J1** (connected by USB cable) | Tested | Not tested yet |
| **ACS ACR1552U** | Tested | Not tested yet |
| Other USB NFC readers that work with the computer's built-in smart-card support (PC/SC) | Should work, but not tested | Should work, but not tested |

- **Tested** means tags were read on a Mac, and the bridge's automatic reader
  recovery was checked with that reader.
- **Windows:** the Windows version is built and released, but it hasn't been
  tested with any reader yet. It also lacks some of the Mac version's features
  (see [Mac and Windows](#mac-and-windows)).
- **Drivers:** the bridge uses the generic smart-card reader driver that comes
  with the operating system, not the drivers the reader's manufacturer (ACS)
  publishes. The readers above were tested only with the driver built into
  macOS, and the bridge's automatic reader recovery is built around it. You
  don't need to install anything extra for the reader.
- **ACR1255U-J1:** use the USB cable. The bridge doesn't use the reader's
  Bluetooth connection.
- **ACR1552U:** leave it in its factory "CCID" mode. Its keyboard mode, where
  the reader types the card number, isn't what the bridge uses.
- **Readers that only work as a keyboard** don't work with the bridge.

**Tags:** NFC Forum Type 2 tags. NTAG215 was tested; NTAG213 and NTAG216 are
supported. Any card's ID (UID) can be read.

## Requirements

- macOS 11 or later (Apple silicon or Intel), or Windows 10 or later (64-bit)
- Google Chrome
- One of the readers above, plugged in by USB

## Mac and Windows

The Mac version is the more complete one. It's the one in daily use, and it can
fix a stuck reader by itself. The Windows version does the same basic job but
can't do that yet.

| | Mac | Windows |
| --- | --- | --- |
| Read and write tags, show tag IDs | Yes | Yes (not tested with a reader yet) |
| Menu, start at login, updates, log upload | Yes | Yes |
| Restart a tag that stops answering | Yes | Yes |
| Notice a reader that beeps but reports nothing, and restart it | Yes | No: unplug and replug the reader |
| Notice a reader that's plugged in but not seen, and restart it | Yes | No: unplug and replug the reader |
| Computer's own USB and reader messages in the logs | Yes | No |

## Install

### Mac

1. Download
   [NFC-Browser-Bridge-macos.dmg](https://github.com/andreykats/nfc-browser-bridge/releases/latest/download/NFC-Browser-Bridge-macos.dmg),
   open it, and drag **NFC Browser Bridge** into **Applications**. Start it from
   Applications, not from the disk image, or it can't start by itself when you
   log in.
2. The first time, macOS blocks it because it isn't signed by Apple yet. Open
   **System Settings → Privacy & Security**, scroll down to the message about
   NFC Browser Bridge and click **Open Anyway**. You only need to do this once.
   Updates from the bridge's menu don't ask again.
3. A card icon appears in the menu bar. **Start at login** turns on by itself.

### Windows

1. Download
   [NFC-Browser-Bridge-windows-setup.exe](https://github.com/andreykats/nfc-browser-bridge/releases/latest/download/NFC-Browser-Bridge-windows-setup.exe)
   and run it. You don't need administrator rights.
2. Windows may warn that the installer is unrecognised: choose **More info →
   Run anyway**.
3. **Start at login** is ticked in the installer. You can change it later in the
   bridge's menu.
4. Windows hides new tray icons. Drag the card icon from the **^** area onto the
   taskbar so it stays visible.

### Allow the website in Chrome

The first time a website uses the bridge, Chrome asks whether it may connect to
**apps on this device**. Choose **Allow**. If you chose Block by mistake, click
the icon to the left of the web address, set **Apps on device** to **Allow**,
and reload the page.

IT administrators can allow a website for everyone with the Chrome policy
`LoopbackNetworkAllowedForUrls` (Chrome 146 and later; earlier versions use
`LocalNetworkAccessAllowedForUrls`). On Windows that's the registry value
`HKLM\SOFTWARE\Policies\Google\Chrome\LoopbackNetworkAllowedForUrls\1 =
"https://example.com"`; on a Mac, a configuration profile for
`com.google.Chrome` with that key set to a list of website addresses.

## The menu

Click the card icon to see:

- **Status:** whether the bridge is ready, each reader that's plugged in, and
  the ID of the tag on it (or "waiting…").
- **Clients:** the websites using the bridge right now.
- **Start at login:** start the bridge when you log in.
- **Open log folder** and **Upload logs:** for troubleshooting (see below).
- **Check for updates:** look for a newer version now. When one is available,
  this item becomes **Update to … and restart**.
- **Quit:** stop the bridge. Websites can't use the reader until you open it
  again.

The icon also shows the status at a glance: no reader, ready, or a tag on the
reader.

## Updates

The bridge checks for a new version when it starts and once a day. When there
is one, the menu shows **Update to … and restart**. Click it, and the bridge
downloads the update, checks that it really comes from us, installs it and
restarts. The reader is unavailable for a few seconds while it does. If the
update can't be installed, a message explains why and offers the download page.

On a Mac, installing an update needs permission to change the Applications
folder, which administrator accounts have.

What changed in each version: [CHANGELOG.md](CHANGELOG.md).

## Security

While the bridge runs, **any website you open in Chrome can use the NFC
reader**. It can see which readers are plugged in, read tags, and write to
them. The bridge refuses the most damaging writes (to the parts of a tag that
can make it unusable), but that's a safety net, not a lock. The menu lists the
websites connected right now. Quit the bridge when you don't need it.

## Troubleshooting

- **A website says the bridge isn't running:** check for the card icon in the
  menu bar or system tray, and start NFC Browser Bridge if it's missing. Also
  make sure Chrome is allowed to connect (see
  [Allow the website in Chrome](#allow-the-website-in-chrome)).
- **The reader beeps but nothing happens, or it stopped working:** on a Mac the
  bridge notices a reader that has stopped working and restarts it by itself
  within a few seconds. If it can't, or on Windows, where the bridge can't do
  this, unplug the reader, wait a few seconds, and plug it back in.
- **"No reader connected":** plug the reader in. The bridge picks it up by
  itself, with no need to restart anything.
- **"Port 38241 in use by another app":** another program is using the port the
  bridge needs. Close that program; the bridge keeps trying every few seconds.
- **The bridge crashed or froze:** it restarts itself. Only **Quit** stops it
  for good.
- **Windows shows "installing device driver" pop-ups, or reading fails with a
  "sharing violation":** ask your IT administrator to turn off Smart Card Plug
  and Play (the DWORD
  `HKLM\SOFTWARE\Policies\Microsoft\Windows\ScPnP\EnableScPnP` set to `0`).
- **Sending us logs:** choose **Upload logs** in the menu. A message shows a
  folder name such as
  `nfc-browser-bridge/Kiosk-Mac-mini-1A2B3C4D/20261108T133654Z_v1.0.10`; send us
  that name. The logs hold the last 7 days of the bridge's activity: readers
  coming and going, the IDs of tags that were tapped, and the addresses of the
  websites that connected. On a Mac they also include the computer's own
  messages about USB and card readers. **Open log folder** shows them on your
  computer.

## For developers

How websites talk to the bridge, the TypeScript client library and the build
instructions are in [docs/PROTOCOL.md](docs/PROTOCOL.md), [client/](client/)
and [CLAUDE.md](CLAUDE.md).

## License

MIT
