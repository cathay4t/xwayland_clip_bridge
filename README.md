# xwayland-clip-bridge

Fixes multi-second freezes when pasting rich content into X11/CEF applications
running under Xwayland (e.g. DingTalk) while the clipboard is owned by a native
Wayland application (Firefox, LibreOffice).

## Root cause

When Firefox (native Wayland) copies formatted text, its Wayland clipboard
offer contains X11-only flavor names such as `SAVE_TARGETS`, `COMPOUND_TEXT`,
`UTF8_STRING`, `TEXT`, `text/_moz_*`.  Xwayland mirrors that offer into the X11
selection and advertises all of those names as X11 `TARGETS`, but it never
delivers data - nor a failure `SelectionNotify` - for some of them.

Applications that enumerate and read every advertised target get stuck:
DingTalk embeds Qt5, and Qt's XCB clipboard blocks for
`QXcbClipboard::clipboard_timeout = 5000` ms per unanswered target.  Result:
~5 s freeze per dead target before the paste completes.  Plain text copies
(foot, address bar) do not include those flavors, which is why they are fast.

## What this daemon does

* Watches the X11 `CLIPBOARD` selection (XFixes, plus polling fallback).
* When the clipboard carries rich content (`text/html`), it reads the useful
  formats from the real owner with short timeouts.
* It then takes ownership of the selection and serves a clean, always
  answerable target set:

  `TARGETS, TIMESTAMP, UTF8_STRING, text/plain;charset=utf-8,
  text/plain, TEXT, STRING, text/html`

* It never advertises `SAVE_TARGETS`/`COMPOUND_TEXT`/`text/_moz_*`, and any
  request for an unknown target is refused immediately with
  `SelectionNotify(property=None)` instead of being left unanswered.

Wayland clients keep working: Xwayland mirrors the daemon's selection back to
the Wayland side.  Plain-text-only clipboards are left untouched.  `INCR`
transfers are supported in both directions for large payloads.

## Build

```sh
cargo build --release
```

The binary is at `target/release/xwayland-clip-bridge`.

## Run

```sh
target/release/xwayland-clip-bridge        # foreground
target/release/xwayland-clip-bridge -v     # verbose
```

## Limitations

* Only text (`text/html` + plain text aliases) is bridged.  Image/file flavors
  are not copied into the bridge; clipboards without `text/html` are left to
  the original owner.
* If the daemon exits while it owns the selection, the previous clipboard
  content is lost (as with any X11 selection owner that exits).
