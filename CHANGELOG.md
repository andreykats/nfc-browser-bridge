# What's new in NFC Browser Bridge

The newest version is at the top. To update, open the bridge's menu (the menu
bar on a Mac, the system tray on Windows) and choose **Update to … and
restart** when it appears, or **Check for updates** to look right away.

## 1.0.11

- On a Mac, the menu bar icon is bigger and now matches the size of the other
  icons next to it. When no reader is connected, the icon shows an X inside
  the card instead of a line through it.

## 1.0.10

- **Check for updates** is new in the menu. It looks for a newer version right
  away and tells you if you're already up to date. If there is a newer one, it
  offers to install it for you.

## 1.0.9

- Fixed: on a Mac, a reader could stop reporting tags while still beeping when
  one was placed on it. Nothing showed up on screen until someone unplugged the
  reader and plugged it back in. The bridge now notices this and restarts the
  reader by itself within a couple of seconds, with no unplugging needed.

## 1.0.8

- Updates can be installed from the menu. When a new version is out, the menu
  shows **Update to … and restart**. The bridge checks that the download really
  comes from us before installing it, then restarts. It's unavailable for a few
  seconds while it does.

## 1.0.7

- The logs now note which reader is plugged in (model, firmware version, serial
  number and USB port), to help track down a problem with one particular
  reader.

## 1.0.6

- Uploaded logs are filed under the computer's name plus a short ID, so logs
  from two computers with the same name don't get mixed up.

## 1.0.4

- The menu shows the ID of the tag on the reader on its own line, or
  "waiting…" when there is no tag.

## 1.0.3

- **Upload logs** is new in the menu. It sends the bridge's logs to us for
  troubleshooting and tells you where they went.
- The menu lists each reader with its tag underneath.
- On a Mac, the bridge notices a reader that is plugged in but has stopped
  working, and restarts it by itself. If that doesn't help, the log says the
  reader needs unplugging and plugging back in.
- The logs keep more of what's needed to figure out problems later: a short
  health check every 10 minutes and, on a Mac, the computer's own messages
  about USB and card readers, which macOS otherwise keeps for only a day or
  two.

## 1.0.2

- The bridge restarts itself if it crashes or gets stuck, so it keeps working
  without anyone having to quit and reopen it.
- It recovers by itself from card reader errors that used to need a restart.

## 1.0.1

- First release. NFC Browser Bridge lets web pages, such as the kiosk, read and
  write NFC tags through a USB reader. It sits in the menu bar on a Mac or the
  system tray on Windows, and starts when you log in.
