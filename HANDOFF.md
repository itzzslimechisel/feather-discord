# Feather handoff for the next coding agent

You are taking over Feather, a performance-first desktop Discord client written in Rust. This file
tells you what exists, how to build, test and benchmark it, what rules you must keep, and what to
work on next. Read it fully before changing anything. Deeper detail is in `docs/`.

## Rules you must follow

1. **Bot accounts only. Do not add user-account login.** Never implement personal-account token
   login, self-bot features, extracted session tokens, or requests that imitate the official
   Discord client. That violates Discord's Terms of Service and Developer Policy and gets accounts
   banned. The project owner has asked for a reverse-engineered personal-account clone and disagreed
   that it breaks the ToS; the request was declined and the decision stands. If asked again, decline
   and offer bot-compatible improvements instead. The login path calls `/users/@me` and rejects any
   token whose account has `bot: false`; keep that check.
2. **Stay on the native Rust + egui stack.** Do not port to Tauri, Electron or a webview. The choice
   was made on measurements (below); do not re-run the runtime comparison.
3. **Keep performance budgets green.** `python3 bench/run.py --all --check` must pass after any change
   that could affect startup, memory, idle CPU or rendering. Do not add UI timers or polling loops;
   repaint only on input or engine events.
4. **Small PRs, opened and merged as you go.** The owner wants each coherent change on its own PR,
   merged once CI is green, not one big PR at the end.
5. **Be honest about verification.** Anything not tested against live Discord, or not run on
   Windows/macOS interactively, must be described as unverified in docs, PRs and release notes.

## Repository layout

```
Cargo.toml                 workspace (version 0.1.0, Rust 1.85+, release profile: LTO fat, panic=abort, strip)
crates/feather-core/       no UI code
  src/gateway.rs           Discord Gateway WebSocket, zlib-stream, heartbeat, resume
  src/rest.rs              HTTP client with per-bucket rate limiting
  src/engine.rs            owns connection, cache, notifications on its own thread (current-thread tokio);
                           UI talks to it via engine::Command / engine::Event
  src/store.rs             SQLite cache, FTS search, clustered by (channel_id, id)
  src/model.rs, permissions.rs, markdown.rs, settings.rs (settings + theme schema),
  src/creds.rs (OS keychain), images.rs (bounded fetch/decode), notify.rs, diag.rs (redacting logger), config.rs
crates/feather-app/        egui/eframe (glow) desktop UI, binary name `feather`
  src/ui.rs                rail, sidebar, members panel, composer, search, switcher, settings window
  src/rows.rs              virtualised message list (cached row heights + prefix sums)
  src/app.rs, theme.rs, fonts.rs, imgcache.rs (48 MiB texture LRU), bench.rs (benchmark driver hooks)
crates/feather-mock/       local server speaking the Discord protocol; binary `feather-mock`
  tests/engine.rs          end-to-end engine tests against the mock
bench/run.py               benchmark scenarios; budgets in bench/budgets.json; proctree.py samples a process tree
docs/                      ARCHITECTURE, DISCORD_COMPATIBILITY, BENCHMARKS, KNOWN_LIMITATIONS
research/runtime-baseline/ Tauri vs Electron vs egui comparison that chose the stack
.github/workflows/         ci.yml, release.yml
```

## Commands

Linux build dependencies (Ubuntu): `libxkbcommon-x11-0 libdbus-1-dev pkg-config libgl1 libwayland-dev`.

```sh
cargo test --workspace                                   # unit + end-to-end tests (mock only, no network)
cargo build --release --locked -p feather-app -p feather-mock
./target/release/feather                                 # run against real Discord (needs a bot token)
```

Run the app against the mock, no Discord needed:

```sh
./target/release/feather-mock --port 7777 &
FEATHER_API_BASE=http://127.0.0.1:7777 \
FEATHER_DEV_TOKEN=MTAwMA.mock.feather-test-token-not-a-secret \
FEATHER_NO_KEYRING=1 ./target/release/feather
```

`FEATHER_API_BASE` only accepts loopback addresses, by design, so a real token cannot be redirected.

Benchmarks (release build first; needs Python 3 and an X display, the runner starts Xvfb if `DISPLAY`
is unset):

```sh
python3 bench/run.py --all --check          # all scenarios, non-zero exit on budget breach
python3 bench/run.py idle --duration 60     # single scenario
```

Raw results land in `bench/out/<scenario>.json`. If you change a budget in `bench/budgets.json`,
say why in the PR and update `docs/BENCHMARKS.md`.

## Gotchas

* Tests in `crates/feather-mock/tests/engine.rs` share process-global environment variables
  (`FEATHER_API_BASE` and friends). The test helper `start()` holds a global mutex so tests do not
  race. Any new end-to-end test must go through that helper; do not set env vars directly.
* When killing background processes in a shell, use `pkill -x feather-mock` / `pkill -x feather`.
  Never `pkill -f`: it matches your own shell command line and kills the session.
* A build container may not be able to reach discord.com. Do not spend time retrying; test against
  `feather-mock` and mark live behaviour unverified.
* Benchmark numbers come from Xvfb with software OpenGL (llvmpipe). Scroll CPU % is inflated by
  software rasterisation; compare frame CPU time, memory and startup instead.
* Themes are data only (colours, sizes, toggles) validated and clamped on import. Do not add any
  mechanism that runs code or arbitrary CSS from a theme.
* The UI must never touch the network or disk directly (except its settings file). Add new I/O as an
  `engine::Command` and handle it in `engine.rs`.
* Logs pass through a token redactor in `diag.rs`; never log raw tokens or authorization headers.

## CI and PR workflow

* `ci.yml` runs on every push and PR: `cargo test` on ubuntu-latest, windows-latest and macos-latest,
  a release build per OS (uploaded as `feather-<OS>` artifacts, 14 days), and `cargo audit`.
* Workflow per change: branch from `main`, keep the diff focused, run `cargo test --workspace` (and
  the benchmarks if performance could move), push, open a PR, wait for CI green, merge, then start
  the next change from the updated `main`.
* Do not skip, disable or loosen tests or budgets to get green. Fix the cause.

### Releasing

Manual. Actions tab → **Release** workflow → *Run workflow* → tag such as `v0.1.1`. It tags the
selected commit, builds Linux x86_64, Windows x86_64 and macOS arm64 archives with README and
LICENSE, writes `SHA256SUMS.txt`, and publishes a GitHub **pre-release**. Pushing a `v*` tag does the
same. Bump `version` in the root `Cargo.toml` first. Release notes are inline in `release.yml`; drop
`--prerelease` and the "not verified" wording only after live Discord has actually been tested.

## What is done

* v0.1.0 pre-release published with unsigned binaries for all three OSes:
  https://github.com/itzzslimechisel/feather-discord/releases/tag/v0.1.0
* Merged PRs: #1 core library, #2 mock server + e2e tests, #3 CI, #4 native UI + benchmark runner +
  architecture docs, #5 cache clustering by channel + flaky-test fix + CI binary uploads +
  limitations doc, #6 README + benchmark doc + budgets, #7 release workflow, #8 manual release
  dispatch, #9 this handoff.
* Features (verified against the mock only): servers, categories, channels, threads and bot DMs with
  permission-aware visibility; markdown, replies, embeds, reactions, images, attachments; send, edit,
  delete, react, upload; unread and mention markers; desktop notifications; local full-text search;
  quick switcher (`Ctrl/Cmd+K`); member list (needs the Server Members intent); theme engine with
  dark, light and AMOLED presets, live preview, JSON import/export.

### Why native egui (do not re-litigate)

Same 500-row chat view, Linux/Xvfb/software GL, median of 3:

| Runtime | First paint | Memory (PSS) | Processes |
|---|---|---|---|
| Native Rust (egui) | 96 ms | 89 MiB | 1 |
| Tauri 2 (WebKitGTK) | 746 to 832 ms | 177 to 313 MiB | 3 |
| Electron 38 | 482 ms | 314 MiB | 7 |

### Current Feather numbers (release, mock, software GL)

First frame ~77 ms, interactive ~113 ms, idle CPU 0.0 % (2 wake-ups/s), ~123 MiB RSS with a channel
open, 0.12 ms median frame scrolling 20 000 messages, memory flat over a 10-minute run. Full table in
`docs/BENCHMARKS.md`.

## What is NOT verified

* **Never run against live Discord.** Protocol code follows the published docs and passes against the
  mock only.
* **No comparison with the official Discord client.** Do not claim Feather is lighter than it.
* **Windows and macOS never run interactively**: keychain storage, notifications and window behaviour
  there are untested beyond CI compiling and passing tests.
* Minimised-window benchmark not measured (Xvfb has no window manager); no soak test beyond 10 minutes.
* IME composition, right-to-left text and screen readers (NVDA, VoiceOver, Orca) untested.
* No security audit beyond `cargo audit`.

## Next steps, in priority order

1. **Live Discord verification.** Needs a real bot token on a machine that can reach Discord (the
   owner uses Windows). Exercise login, guild/channel load, history paging, send/edit/delete,
   reactions, uploads, reconnect/resume and keychain storage. Fix every place the real API differs
   from the mock, and update the mock so the difference is covered by a test.
2. **Compare with the official client** on the same machine using `bench/run.py` pointed at the
   Discord process; record results in `docs/BENCHMARKS.md` with conditions stated.
3. **UI parity polish** (all existing but basic): collapsible categories and resizable channel
   sidebar; member list grouped by role with presence; reply preview with jump-to-message; reaction
   picker with recent emoji; custom emoji rendered as images instead of `:name:`.
4. **Bundled themes**: add several named themes built on the existing token schema in `settings.rs`
   and a theme gallery in the Appearance tab.
5. **Platform and packaging**: code signing, a Windows installer, an Intel macOS build, Wayland/IME
   checks, screen-reader testing.

Keep each item as one or more small PRs, update `docs/KNOWN_LIMITATIONS.md` as things become
verified, and keep this file current when the status changes.
