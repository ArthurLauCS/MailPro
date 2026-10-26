# MailPro

**Read your mail without telling the sender.**

English | [简体中文](README.zh-CN.md)

A privacy-focused desktop mail client for Windows, written in Rust with a native GPU-rendered interface (gpui). Nothing remote loads by default, so a sender cannot learn from a message when or where you opened it. Each account connects through its own SOCKS5 proxy and never falls back to a direct connection.

## Features

1. **Tracking blocked by default.** Message HTML goes through an allowlist sanitizer; scripts, frames, forms and remote resources are removed or replaced with local placeholders. The body renders in WebView2 with scripts off and a `default-src 'none'` content security policy, and every request it makes is answered locally, so the engine itself never touches the network.
2. **Remote images only when you allow them**, for one message or always for a sender. MailPro fetches them itself through the account's proxy, without cookies or a referrer and with a generic User-Agent. Fonts, style sheets and media stay blocked. Images are not cached: each load is visible to the sender again, from the proxy's address.
3. **See who is tracking.** The reader header lists the domains of blocked and loaded remote resources with their counts.
4. **A proxy per account.** SOCKS5 `host:port`; host names are resolved by the proxy, so no mail server name reaches your DNS. If the proxy is down, the connection fails instead of going direct. Accounts without a proxy are marked "Direct" in their settings.
5. **Safe links.** Clicking a link shows the cleaned address first: known redirect wrappers (Google, Outlook SafeLinks, Facebook and others) are unwrapped and tracking parameters removed, with the original address listed next to it. Open it in the system browser, a program you choose, or an [AdsPower](https://www.adspower.com/) profile, which is started through AdsPower's local API so its fingerprint and proxy apply.
6. **Gmail and other IMAP accounts.** Gmail signs in with an app password; other providers with a manually entered IMAP server over TLS. Read status lives on the server, so several computers stay in sync: marking a message read is written back within seconds, and changes made elsewhere reach the inbox within seconds (IDLE) and other folders within five minutes.
7. **Fast with large folders.** Headers of every folder are synced in full and kept in a local SQLite database; bodies are downloaded when opened. A folder of 50,000 messages scrolls at a median frame time of 2.8 ms.
8. **No telemetry, no crash reporting.** English and Simplified Chinese; light and dark themes follow the system, and dark mode darkens message bodies too.

Checks behind these claims: a test message carrying 31 tracking techniques triggers all 31 in an unprotected headless Chrome and none in MailPro; the [emailprivacytester.com](https://www.emailprivacytester.com/) test shows no new hits until images are loaded, and then only the proxy's address; during ten minutes of use the WebView2 processes opened no outside connection.

## Download and install

**[Download the installer](https://github.com/ArthurLauCS/MailPro/releases/download/v0.1.0/MailPro-Setup-0.1.0.exe)** · [SHA256 checksums](https://github.com/ArthurLauCS/MailPro/releases/download/v0.1.0/SHA256SUMS.txt)

Run `MailPro-Setup-0.1.0.exe` from [GitHub Releases](https://github.com/ArthurLauCS/MailPro/releases/tag/v0.1.0). It installs for the current user under `%LOCALAPPDATA%\Programs\MailPro` and needs no administrator rights.

- Windows 10 / 11 x64 with the [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/) (included in Windows 11 and in up-to-date Windows 10).
- The installer offers English and Simplified Chinese; the app's language is chosen separately in the app.
- The installer is unsigned, so Windows SmartScreen may warn about an unknown publisher: choose **More info → Run anyway**. Check the file against `SHA256SUMS.txt` with `Get-FileHash .\MailPro-Setup-0.1.0.exe -Algorithm SHA256`.

Mail and settings are stored in `%LOCALAPPDATA%\MailPro`; passwords are stored in Windows Credential Manager. Uninstalling removes both.

## Usage

1. **Add account** (bottom of the left column): enter the address, an app password and, recommended, a SOCKS5 proxy such as `127.0.0.1:7890`. For Gmail, turn on 2-Step Verification and create an app password at [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords). For other providers, also enter the IMAP server and port (TLS only).
2. Opening a message marks it read on the server. The reader header lists the blocked remote content; **Load images** loads it for this message, **Always load images from this sender** for every message from that address.
3. **Browser** (bottom of the left column) chooses where links open. For AdsPower, enter the profile ID and the API key from AdsPower's API settings; the key is kept in Credential Manager.
4. Drag an account row onto another account to reorder them. An account row glows softly while its inbox has mail received in the last 30 minutes.
5. Click a message to open it; Ctrl-click adds or removes one, Shift-click selects a range. Archive, delete, move, mark read / unread and star from the buttons at the top of the reader or from the right-click menu; the button at the top of the list marks the whole folder read. Delete moves to Trash, and inside Trash deletes forever after a confirmation. Changes are written to the server; moving and deleting need the account to be connected.
6. The search box at the top of the list finds messages whose subject or sender contains every word you type. The button next to it switches between **This folder** and **All mailboxes** (every folder of every account; results show their folder). Message bodies are not searched. Clicking a folder clears the search.

## Current limits

Not yet available: writing and sending, Outlook / Hotmail accounts (their IMAP requires OAuth), desktop notifications, and import / export of settings.

## Development

Requires Rust stable with the MSVC C++ tools; [Inno Setup 6](https://jrsoftware.org/isinfo.php) for the installer.

```powershell
.\scripts\fonts.ps1                           # downloads the two bundled fonts (not in git); without them the UI uses system fonts
cargo run -p mailpro                          # the app
cargo run -p mailpro -- C:\path\to\message.eml  # shows one message file
cargo test --workspace
cargo run -p mailpro-core --bin mailpro-cli   # terminal tool for accounts and sync, usage at the top of its source
.\scripts\installer.ps1                       # installer: target\installer\MailPro-Setup-<version>.exe
```

- Privacy check: `python tests/privacy/tracker.py serve <log>` starts a local hit counter; open `tests/privacy/tracking.eml` in MailPro (`scripts/drive.ps1` runs it without taking focus) and the log must stay empty.
- IMAP sync test against a local GreenMail server: see the top of `crates/core/tests/sync.rs`.
- The app icon comes from `crates/app/assets/icon.svg` and `icon-small.svg`; after editing them run `python scripts/icon.py`. After dependency changes run `python scripts/notices.py`.

| Path | Responsibility |
| --- | --- |
| `crates/core/` | Mail logic without UI: parsing, HTML sanitizing, link cleaning, IMAP, proxied HTTP, SQLite store, credentials, sync engine |
| `crates/app/` | gpui interface and the WebView2 message viewer |
| `tests/privacy/` | Tracking test message and local hit counter |
| `installer/` | Inno Setup script |
| `docs/PLAN.md` | Plan, decisions and status (Chinese) |

## Feedback and licenses

Report problems in [Issues](https://github.com/ArthurLauCS/MailPro/issues) with the version, Windows version and steps to reproduce. Remove addresses and message contents before sharing logs.

Third-party notices are in [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt) and included in the installation. MailPro is licensed under the [MIT License](LICENSE).
