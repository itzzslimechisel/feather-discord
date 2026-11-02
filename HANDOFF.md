# Feather handoff

Read this first if you are picking up Feather. It summarises what exists, why it is built the way it
is, what has and has not been verified, and where to go next. Details live in the linked docs.

## Goal

A performance-first desktop chat client that talks to Discord through **supported interfaces only**.
The brief ranked priorities as: correct and permitted connectivity, near-zero idle CPU, low memory,
fast startup, smooth scrolling, minimal background activity, a familiar UI, and free local themes.

## Stack: native Rust + egui, chosen by measurement

The brief suggested Tauri as a starting hypothesis. The same 500-row chat view was built in three
runtimes and measured (Linux, Xvfb, software GL, median of 3; full data in
[`research/runtime-baseline/RESULTS.md`](research/runtime-baseline/RESULTS.md)):

| Runtime | First paint | Memory (PSS) | Processes |
|---|---|---|---|
| Native Rust (egui/eframe, glow) | **96 ms** | **89 MiB** | 1 |
| Tauri 2 (WebKitGTK) | 746 to 832 ms | 177 to 313 MiB | 3 |
| Electron 38 | 482 ms | 314 MiB | 7 |

Idle CPU was near zero for all three, so startup and memory decided it. The costs accepted with
native: no arbitrary CSS themes (themes are validated design tokens instead), less rich text
layout than a browser, and accessibility that depends on egui's AccessKit support. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Architecture in one screen

| Crate | Role |
|---|---|
| `crates/feather-core` | Everything except drawing: Gateway (WebSocket + zlib-stream), rate-limited REST, `engine` (own thread, current-thread tokio runtime), SQLite cache with full-text search, permissions, markdown parser, settings and theme format, OS keychain (`creds`), bounded image fetch, redacting logger. |
| `crates/feather-app` | egui UI: server rail, channel sidebar, virtualised message list (`rows.rs`), composer, member list, search, quick switcher, settings window. Never touches the network; it sends `engine::Command`s and receives `engine::Event`s over a channel. |
| `crates/feather-mock` | Local server that speaks the Discord Gateway/REST protocol. Used by the end-to-end tests (`tests/engine.rs`) and every benchmark. |
| `bench/` | `run.py` scenario runner, `budgets.json`, `proctree.py` process-tree memory/CPU sampler. |

Why it idles quietly: no UI timers (egui repaints only on input or engine events), the only
recurring timer is the protocol heartbeat, and disk flushes are debounced and batched. Memory is
bounded everywhere (400 in-memory messages for the open channel only, 48 MiB texture LRU, capped
queues and log file). The bounds table is in ARCHITECTURE.md.

## Current status

* **v0.1.0 is published as a pre-release**: [releases/tag/v0.1.0](https://github.com/itzzslimechisel/feather-discord/releases/tag/v0.1.0),
  with Windows, Linux and macOS (arm64) archives plus `SHA256SUMS.txt`. Binaries are unsigned.
* Merged PRs, in order: #1 core library, #2 mock server and end-to-end tests, #3 CI, #4 native UI,
  benchmark runner and architecture docs, #5 cache clustering by channel, flaky test fix, CI binary
  uploads and limitations doc, #6 README, benchmark doc and budgets, #7 release workflow, #8 manual
  release dispatch.
* **CI** (`.github/workflows/ci.yml`) runs `cargo test` on Ubuntu, Windows and macOS, builds a release
  binary per OS (uploaded as `feather-<OS>` artifacts, kept 14 days) and runs `cargo audit`.
* Features implemented (against the mock): servers, categories, channels, threads and bot DMs with
  permission-aware visibility; markdown, replies, embeds, reactions, images and attachments; send,
  edit, delete, react, upload; unread and mention markers; notifications; local search; quick
  switcher; theme engine with dark, light and AMOLED presets, live preview and JSON import/export.

## Benchmarks

Headline numbers (release build, mock server, software rendering): first frame ~77 ms,
interactive ~113 ms, idle CPU 0.0 % with 2 wake-ups/s, ~123 MiB RSS with a channel open, 0.12 ms
median frame cost scrolling 20 000 messages, memory flat over a 10-minute run. All ten scenarios,
conditions and caveats are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

To rerun:

```sh
cargo build --release -p feather-app -p feather-mock
python3 bench/run.py --all --check          # every scenario; fails if bench/budgets.json is exceeded
python3 bench/run.py idle --duration 60     # one scenario
```

Needs Python 3 and an X display (the runner starts Xvfb if none is set). Raw output goes to
`bench/out/<scenario>.json`.

## Known limitations and unverified items

Be explicit about these with anyone you report to. Full list: [docs/KNOWN_LIMITATIONS.md](docs/KNOWN_LIMITATIONS.md).

* **Never run against live Discord.** The build container cannot reach discord.com. The protocol code
  follows the published docs and is tested only against `feather-mock`, so real-API surprises are
  possible.
* **No comparison with the official Discord client.** Any "lighter than Discord" claim is unverified.
  `bench/run.py` can be pointed at another process to do this on a real machine.
* **Windows and macOS are untested beyond CI.** They compile and pass tests there, but the keychain
  paths, notifications and window behaviour were never run interactively.
* All benchmarks used software OpenGL (llvmpipe) in Xvfb. Scroll CPU % is inflated; memory and
  startup are more representative. The minimised-window scenario was not measured (no window manager).
* No overnight soak test; the long run was 10 minutes.
* Not security audited beyond `cargo audit`.

## Bot-token-only decision

Feather signs in only as a Discord **bot** the user creates and owns. After a token is pasted it
calls `/users/@me` and refuses any account that is not a bot.

Why: logging in with a personal account token, or reverse engineering and mimicking the official
client's requests, is against Discord's Terms of Service and Developer Policy, and Discord enforces it
with account bans. The original brief also forbade self-bots, extracted tokens and imitating the
official client.

After v0.1.0 the project owner asked for a reverse-engineered clone that signs into a personal
account and behaves exactly like the official app. They disagreed that this breaks the ToS. That
request was **declined** and the bot-only design stands. Do not add user-token login, request
spoofing or similar. Consequences for users: only servers the bot is invited to are visible, no
personal friends or DMs, and the Message Content intent (and Server Members for the member list) must
be enabled in the Developer Portal. See [docs/DISCORD_COMPATIBILITY.md](docs/DISCORD_COMPATIBILITY.md).

## Release process

Releases are manual:

1. Make sure CI is green on `main`.
2. Actions tab → **Release** workflow → *Run workflow* → enter a tag such as `v0.1.1`.
3. The workflow creates the tag on the selected commit, builds Linux x86_64, Windows x86_64 and
   macOS arm64, packages them with README and LICENSE, writes SHA256 checksums and publishes a
   GitHub pre-release.

Pushing a `v*` tag triggers the same workflow. Release notes are inline in
`.github/workflows/release.yml`; update them (and drop `--prerelease`) once live Discord is verified.
Bump `version` in the root `Cargo.toml` with each release.

## Suggested next steps

1. **Live test with a real bot token** on the owner's Windows machine: login, history, sending,
   reactions, uploads, reconnects, keychain storage. Fix whatever the real API does differently from
   the mock. This is the highest-value item.
2. **Measure against the official client** on that same machine with `bench/run.py`, so performance
   claims can be stated rather than hedged.
3. **Make the UI feel closer to the official app.** The sidebar, member list, replies and reactions
   exist but are basic: polish the channel sidebar (collapsible categories, drag widths), member list
   (role grouping, presence), reply previews and jump-to, a reaction picker with recent emoji, and
   custom emoji images instead of `:name:` text.
4. **Bundled themes.** Only the three base presets ship. Add a handful of named themes built on the
   existing token format, and a simple theme gallery in settings.
5. Platform polish: signed binaries, an installer for Windows, an Intel macOS build, a Wayland/IME
   check, and screen-reader testing.

## Working conventions

* Keep PRs small; the owner wants them opened and self-merged as work progresses, not batched.
* `cargo test --workspace` before pushing. Tests in `feather-mock/tests/engine.rs` share
  process-global env (`FEATHER_API_BASE`), so the mock `start()` holds a mutex.
* Try the app without Discord: run `feather-mock --port 7777` and start Feather with
  `FEATHER_API_BASE=http://127.0.0.1:7777`, `FEATHER_DEV_TOKEN` and `FEATHER_NO_KEYRING=1` (exact
  command in the README). `FEATHER_API_BASE` accepts loopback addresses only.
