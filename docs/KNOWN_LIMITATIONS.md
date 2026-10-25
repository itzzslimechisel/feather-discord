# Known limitations

Written to be read before trusting any claim in this repository.

## Discord access
* **Bot accounts only.** There is no supported way for a third-party client to act as a personal
  account, so Feather cannot show your own servers, friends or DMs. See
  [DISCORD_COMPATIBILITY.md](DISCORD_COMPATIBILITY.md).
* Reading server messages needs the *Message Content* privileged intent; the member list needs
  *Server Members*. Without them the UI still works but shows blank or partial content, and the app
  says so.
* No voice, video, stickers, activities, group DMs or friend list (not available to bots).
* Search covers only messages already cached on this machine.
* Custom emoji appear as `:name:` text; animated avatars and GIF embeds are shown as still images.
* **Nothing has been run against the real Discord service.** The build environment could not reach
  discord.com. The Gateway/REST code follows the published protocol and is tested against a mock
  that implements the same protocol, so field-level surprises in the real API are possible.

## Platforms
* Developed and measured on Linux (Xvfb with software rendering). Windows and macOS build in CI but
  were **not run** by me: the Windows Credential Manager and macOS Keychain paths, native
  notifications there, and window behaviour are unverified.
* Wayland/X11 input-method (IME) composition relies on egui's support and is untested.

## Rendering and text
* egui draws text without a browser's shaping depth. Right-to-left and complex scripts depend on the
  fonts available; CJK/emoji coverage needs the optional "use system fonts" setting (costs memory).
* Bold needs a system bold font; otherwise it falls back to a brighter colour.
* Screen-reader support goes through AccessKit and egui's widget tree. It exposes controls and
  labels, but the virtualised message list does not expose off-screen messages, and it has not been
  tested with NVDA, VoiceOver or Orca.

## Benchmarks
* All numbers come from a virtual display with software OpenGL (llvmpipe). Software rasterisation
  inflates CPU during scrolling by a large factor; the GPU-bound part of those figures should not
  be read as what a normal machine would see. Memory and startup are more representative than
  scroll CPU.
* The "minimised window" scenario cannot be driven without a window manager and is **not measured**.
* **No comparison with the official Discord client is made.** It could not be installed or measured
  here. Any such comparison would have to be done on the target machine; the harness
  (`bench/run.py`) is built to be pointed at another process for that purpose, but I have not done it.
* Payload sizes are for the mock's generated data, not real Discord traffic.

## Security
* Not independently audited. `cargo audit` runs in CI; dependency findings are listed in the
  workflow output, not suppressed.
* The token lives in the OS credential store when available. If none is available (some headless
  Linux setups) Feather asks again each launch rather than writing the token to a file.
