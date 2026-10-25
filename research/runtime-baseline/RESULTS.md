# Runtime baseline (Phase 1)

Same workload in each shell: one 1000x700 window showing 500 two-part chat rows
(`page/index.html` for the web shells, `egui/src/main.rs` for native). Measured with
`measure.py` (3 trials each, 5 s settle, 20 s idle window). Memory is summed over the
whole process tree. Figures are medians (min/max in parentheses).

Environment: Ubuntu 24.04 container, 4 vCPU, 16 GB, Xvfb :99 1920x1080x24, Mesa llvmpipe
(software GL, no GPU), WebKitGTK 2.52.6, Electron 38, eframe/egui 0.33 (glow), Rust 1.97 release
builds (opt-level=s, LTO, strip). Measured 2026-10-25.

| Shell | Launch to first paint | PSS | USS (private) | RSS | Procs | Threads | Idle CPU (1 core) |
|---|---|---|---|---|---|---|---|
| Native egui (eframe/glow) | **96 ms** (86-110) | **89 MiB** | **68 MiB** | 110 MiB | 1 | 11 | 0.0 % |
| Tauri 2 / WebKitGTK default | 832 ms (800-4273) | 313 MiB | 225 MiB | 465 MiB | 3 | 59 | 0.05 % |
| Tauri 2, `WEBKIT_DISABLE_DMABUF_RENDERER=1` | 746 ms | 177 MiB | 102 MiB | 315 MiB | 3 | 37 | 0.05 % |
| Electron 38 | 482 ms (438-686) | 314 MiB | 181 MiB | 601 MiB | 7 | 71 | 0.0 % |

Notes and caveats
* All shells were idle-quiet once painted; idle CPU does not separate them. Memory and startup do.
* llvmpipe inflates GL memory for every shell. Absolute numbers on a real GPU will be lower, and on
  Windows Tauri uses WebView2 (Chromium-based, multi-process) rather than WebKitGTK, so its
  footprint there is closer to Electron's process model. Neither could be measured here.
* Electron's distribution is 285 MB on disk; the Tauri and egui binaries are 3.4 MB and 3.9 MB.
