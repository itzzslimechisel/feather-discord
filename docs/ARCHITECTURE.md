# Architecture

## Stack choice

Feather is a native Rust application (egui/eframe with an OpenGL backend). The decision was made
from measurements, not preference. The same 500-row chat view was built in Tauri 2, Electron 38 and
native egui and measured with `research/runtime-baseline/measure.py` (Linux, Xvfb, software GL,
three trials each; process-tree memory):

| Shell | Launch to first paint | PSS | USS | Processes | Threads |
|---|---|---|---|---|---|
| Native egui | 96 ms | 89 MiB | 68 MiB | 1 | 11 |
| Tauri 2 (WebKitGTK, tuned) | 746 ms | 177 MiB | 102 MiB | 3 | 37 |
| Tauri 2 (WebKitGTK, default) | 832 ms | 313 MiB | 225 MiB | 3 | 59 |
| Electron 38 | 482 ms | 314 MiB | 181 MiB | 7 | 71 |

Idle CPU was ~0 % for all of them, so memory and startup decide. A webview shell pays for a whole
browser engine (multiple processes, JS heap, layout engine, GPU process) before any application code
runs. The measurements above are for Linux; on Windows Tauri uses WebView2 (Chromium) with a
process model closer to Electron's, which could not be measured here.

Costs of the native choice, accepted deliberately:

* No arbitrary CSS themes. Themes are structured design tokens (colours, sizes, toggles) that are
  validated on import, which is also what makes them safe to share.
* egui has no rich-text layout engine of the web's depth. Markdown is parsed to styled spans and laid
  out as one text galley per message; bold needs a system bold font (it falls back to a brighter colour);
  custom emoji are shown by name; non-Latin scripts beyond egui's bundled fonts need the optional
  system-font setting.
* Accessibility depends on AccessKit support in egui, which is enabled, but is not a full match for
  a browser's accessibility tree.

## Crates

* `feather-core`: everything that is not drawing. Modules: `gateway` (WebSocket protocol),
  `rest` (rate-limited HTTP), `engine` (owns connection, cache and notification logic on its own
  thread with a current-thread async runtime), `store` (SQLite), `markdown`, `permissions`, `settings`
  (settings and theme format), `creds` (OS keychain), `images` (bounded fetch and decode), `diag`
  (redacting logger, startup marks).
* `feather-app`: the egui UI. It never touches the network or disk (other than its settings file);
  it sends `engine::Command`s and receives `engine::Event`s through a channel and wakes itself with
  `request_repaint` when an event arrives.
* `feather-mock`: a Discord-protocol mock for tests and benchmarks.

## Why it idles quietly

* No UI timers. egui repaints only on input or when the engine posts an event.
* The engine's only recurring timer is the Gateway heartbeat (about every 41 s), which the protocol
  requires. History flushes to disk are debounced (1.5 s) and batched into one transaction.
* Blinking cursor, animations and the settings-save debounce are off or one-shot by default.

## Memory bounds

| Resource | Bound |
|---|---|
| Messages in memory | At most 400 rows, only for the open channel; others are only counters |
| Message history on disk | Configurable, default 2000 per channel; pruned in batches |
| Image textures | 48 MiB LRU; decoded at display size with an 8192 px/256 MiB decode limit and 16 MiB download cap |
| Gateway buffers | zlib output buffer shrinks after large payloads; command queue capped at 64 |
| REST bucket table | Expired entries dropped above 256 |
| Log file | Truncated above 1 MiB |

## Rendering

The message list is virtualised with exact row heights: every row is laid out once per width/theme
generation (`rows::build`), prefix sums locate the visible window, and only rows within the viewport
(plus overscan) are painted and keep their text layouts. Scroll position is anchored to a row when
older history is prepended. Older and newer pages come from SQLite first and the network second.

## Security model

* Token: stored only in the OS credential store; zeroised in memory; never logged (log lines pass
  through a redactor); only sent to the configured API/Gateway hosts. A token that belongs to a user
  account is refused.
* Untrusted content: message text becomes styled text, mentions or validated `http(s)` links only.
  Links open after a confirmation that flags look-alike hosts. Images load only from Discord CDN
  hosts, anonymously, with size limits.
* Themes and settings are data; imports are size-limited, parsed into a closed schema and clamped.
* Corrupt settings or cache files are moved aside and rebuilt; they never block startup.
* The test/benchmark endpoint override only accepts loopback addresses, so a real token cannot be
  redirected to an arbitrary host through an environment variable.
