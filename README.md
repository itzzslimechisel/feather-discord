# Feather

A performance-first desktop chat client that connects to Discord through **supported interfaces only**.
Feather is a native Rust application (no browser engine, no Electron), built to stay fast to start,
small in memory and silent when idle.

> **Status: working, unverified against live Discord.** Everything is implemented and tested
> against a local mock of the Discord protocol (`feather-mock`). The build environment could not reach
> discord.com, so first contact with the real service is still to be done. See
> [known limitations](docs/KNOWN_LIMITATIONS.md).

## What it does

* Servers, categories, channels, threads and bot DMs, with permission-aware channel visibility.
* Message list with markdown, replies, embeds, reactions, images and attachments; history paging
  from a local SQLite cache; local full-text search.
* Send, edit, delete, reply, react, upload files.
* Unread and mention markers, desktop notifications, quick switcher (`Ctrl/Cmd+K`), keyboard
  shortcuts.
* Local theme engine (free): dark, light and AMOLED bases, accent colour, font size, density,
  message spacing, sidebar width, corner radius, background (solid or gradient), reduced motion,
  live preview, import/export as JSON, reset to defaults.

## Performance (release build, mock server, software rendering)

| | |
|---|---|
| First frame | ~77 ms |
| Interactive | ~113 ms |
| Idle CPU | 0.0 % (2 wake-ups/s) |
| Memory with a loaded channel | ~123 MiB RSS, 1 process |
| Frame cost while scrolling 20 000 messages | 0.12 ms median, 0.20 ms p95 |

Method, conditions and every scenario are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md). These are
measurements of Feather alone: **no comparison with the official Discord client has been made.**

## How Feather connects to Discord

Feather signs in as a **Discord bot account** that you create and own. This is the only way a
third-party client can send and receive messages while staying within Discord's Terms of Service.

* Logging in with a personal account token ("self-bots") is **not supported**, and Feather refuses
  user tokens: after you paste a token it checks `/users/@me` and rejects anything that is not a bot.
* Your bot only sees servers it has been invited to, and DMs with users who share one of those servers.
* Reading message text in servers requires the privileged **Message Content** intent, enabled in the
  [Developer Portal](https://discord.com/developers/applications) under *Bot → Privileged Gateway Intents*.
  The member list needs the **Server Members** intent.

### Setting up a bot

1. Create an application at <https://discord.com/developers/applications> and add a Bot.
2. Enable *Message Content Intent* (and optionally *Server Members Intent*).
3. Invite it to your server with OAuth2 → URL Generator, scope `bot`, permissions *View Channels*,
   *Send Messages*, *Read Message History*, *Add Reactions* and *Attach Files*.
4. Copy the bot token into Feather. It is stored in your OS keychain (Windows Credential Manager,
   macOS Keychain or Secret Service on Linux), never in a plain file.

## Why native Rust instead of Tauri or Electron

The same 500-message view was measured in each runtime (median of 3 runs, Linux and Xvfb with software
rendering; see [`research/runtime-baseline/RESULTS.md`](research/runtime-baseline/RESULTS.md)):

| Runtime | Launch to first paint | Total memory (PSS) | Processes |
|---|---|---|---|
| Native Rust (egui) | **96 ms** | **89 MiB** | 1 |
| Tauri 2 (WebKitGTK) | 746 to 832 ms | 177 to 313 MiB | 3 |
| Electron 38 | 482 ms | 314 MiB | 7 |

All three used negligible CPU when idle. These are runtime overheads, not Feather's own numbers, and
they haven't been compared with the official Discord client.

## Repository layout

| Path | Contents |
|---|---|
| `crates/feather-core` | Discord model, permissions, gateway, REST with rate limiting, SQLite cache with full-text search, keychain storage, markdown parser, settings and themes. No UI code. |
| `crates/feather-app` | Native desktop UI (egui). |
| `crates/feather-mock` | Local Discord-protocol mock server for tests and benchmarks. |
| `bench/` | Benchmark runner (`run.py`), budgets (`budgets.json`) and process-tree measurement helpers. |
| `docs/` | Architecture, Discord compatibility, benchmarks, known limitations. |
| `research/` | Runtime comparison used to choose the stack. |

## Building and running

Requires Rust 1.85 or newer.

```sh
cargo test --workspace             # unit and end-to-end tests (against the mock)
cargo build --release              # optimised, reproducible with the committed Cargo.lock
./target/release/feather           # run
```

On Linux you also need the development packages for X11/Wayland and D-Bus
(`libxkbcommon-x11-0`, `libdbus-1-dev`, `pkg-config`, `libgl1`).

**Prebuilt binaries:** every CI run uploads `feather` (Linux, macOS) and `feather.exe` (Windows) as
workflow artifacts (Actions tab, kept 14 days). They are unsigned.

### Trying it without Discord

```sh
cargo build --release -p feather-app -p feather-mock
./target/release/feather-mock --port 7777 &
FEATHER_API_BASE=http://127.0.0.1:7777 FEATHER_DEV_TOKEN=MTAwMA.mock.feather-test-token-not-a-secret \
  FEATHER_NO_KEYRING=1 ./target/release/feather
```

`FEATHER_API_BASE` only accepts loopback addresses, so a real token cannot be redirected elsewhere
this way.

### Benchmarks

```sh
python3 bench/run.py --all --check   # runs every scenario, fails if a budget is exceeded
```

## More documentation

* [Architecture and stack choice](docs/ARCHITECTURE.md)
* [Discord compatibility and policy notes](docs/DISCORD_COMPATIBILITY.md)
* [Benchmarks](docs/BENCHMARKS.md)
* [Known limitations](docs/KNOWN_LIMITATIONS.md)

## Security

* Bot tokens are kept in the OS credential store; if none is available they live in memory for the
  session only.
* Logs pass through a redactor that removes anything token-shaped.
* Message content is parsed into styled text only; links must be `http(s)` and are confirmed before
  they open. Images load only from Discord's CDN hosts, without credentials, with size and dimension caps.
* Themes are plain data (colours and sizes), validated and clamped on import. They can't run code.

## License

Apache-2.0. Feather is an independent project and is not affiliated with or endorsed by Discord.
