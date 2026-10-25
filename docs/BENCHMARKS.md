# Benchmarks

Everything here is produced by `bench/run.py` against `feather-mock` (a local server that speaks
the Discord protocol) with the **release** build. Raw output for each run is written to
`bench/out/<scenario>.json`; budgets live in `bench/budgets.json` and `python3 bench/run.py --all
--check` fails when one is exceeded.

## Conditions (read before using any number)

* Linux container, Xvfb 1280x800, **software OpenGL (llvmpipe)**, no GPU, no window manager.
* Medians of 3 trials for the startup scenarios (min-max shown); a single 30 s window for the others.
* Memory is **RSS** for budgets because it was stable between runs; PSS/USS from the same runs
  moved by ~25 MiB between sessions with identical RSS (shared-page accounting in this container),
  so treat PSS/USS as indicative only.
* CPU % for scenarios that repaint continuously (scroll, switching) is dominated by llvmpipe
  rasterising in software on 4 threads. Frame CPU time (the time Feather itself spends building a
  frame) is the number to look at there.
* Nothing was compared against the official Discord client. It was not available here. **Any
  statement that Feather is lighter than Discord's client is therefore unverified.** The only
  comparison made is against the same chat view built in Tauri and Electron (see
  [ARCHITECTURE.md](ARCHITECTURE.md)).

## Results (release build, mock server)

| # | Scenario | What it does | Result |
|---|---|---|---|
| 1 | Cold start | empty cache, login, first content | first frame **79 ms** (70-95), interactive **113 ms** (98-121), 13 threads, 123 MiB RSS |
| 2 | Warm start | cache from a previous run, mock reachable | first frame **77 ms**, cached messages on screen **118 ms** |
| 3 | Idle | logged in, window visible, 30 s | CPU **0.0 %**, **2 context switches/s**, 0 disk writes, 123 MiB RSS |
| 4 | Active chat | 2 messages/s into the open channel | 6.1 % CPU (llvmpipe), frame CPU p50 0.20 ms / p95 0.32 ms, 18 KiB/s disk |
| 5 | Large channel | 20 000-message history, continuous scrolling | 133 MiB RSS, frame CPU p50 0.12 ms / p95 0.20 ms / max 10.4 ms |
| 6 | Many servers | 25 servers x 40 channels, switching every 0.5 s with chatter | 129 MiB RSS, frame p95 1.3 ms, **385 KiB/s** disk writes (was 1136 before the cache layout change) |
| 7 | Images | 600 messages with images, scrolling | 200 MiB RSS peak (texture cache capped at 48 MiB), frame p95 0.30 ms |
| 8 | Minimised | | **not measured**: needs a window manager that Xvfb does not provide |
| 9 | Channel switching | 3 servers, switch every 150 ms | frame p95 1.8 ms, 125 MiB RSS |
| 10 | Reconnect | 5 abrupt connection drops | 5/5 recovered by session resume, no re-identify |

### Long run

10 minutes of 5 msg/s chatter across 5 channels plus a channel switch every 0.8 s (5 servers x 10
channels, 500 messages each, image channel included): RSS went 127.5 MiB at start to 130.1 MiB
after 90 s and stayed at exactly that value to the end, 13 threads throughout; fitted growth over the
second half is 0.0 MiB/hour. Ten minutes shows the caches are bounded, but it is not an
overnight soak test, which I did not run.

## What the numbers say, and what they don't

* Startup and idle are where the native stack pays off. 0 % idle CPU follows from having no timers
  except the protocol heartbeat (about every 41 s against real Discord).
* Frame cost stays around 0.1-0.3 ms at the median because only visible rows are laid out and
  painted and their heights are cached; 20 000 messages cost the same per frame as 300.
* The one real outlier is the 10 ms worst frame in the large-channel run: that is a page of older
  history being measured and inserted. It is a single frame, not a steady cost.
* Software rendering inflates the CPU % of anything that repaints continuously. On a machine with a
  GPU the same frames are much cheaper; I could not measure that here.
* The mock sends small, synthetic messages. Real servers have larger payloads (embeds, many
  members); the network rows (bytes on the wire, requests) are for the mock only.

## Optimisation done because of these runs

* **Disk writes** in the many-servers scenario were 1136 KiB/s. Message ids are global snowflakes, so
  each channel's page of history landed on scattered table pages. The cache table is now clustered
  by `(channel_id, id)`, which cut writes to 385 KiB/s, and unchanged rows are skipped on
  re-fetch.
* Idle CPU, memory growth and frame times needed no changes: they were already inside budget.

## Reproducing

```sh
cargo build --release -p feather-app -p feather-mock
python3 bench/run.py --all --check          # all scenarios, fails on budget breach
python3 bench/run.py idle --duration 60     # one scenario
```

Needs Python 3 and an X display (the script starts Xvfb if none is set).
