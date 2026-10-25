# Discord compatibility and policy notes

Feather connects as a **bot account** using Discord's documented Gateway (v10) and REST API. This
page records what that does and does not allow. Sources are Discord's developer documentation
(Gateway, Opcodes and Status Codes, Rate Limits, Permissions, Message Content intent) and Discord's
published stance on automating user accounts. Nothing below has been exercised against the live
service yet: all connectivity tests in this repository run against `feather-mock`, a local server
that implements the documented protocol (see [Verification status](#verification-status)).

## What Feather will not do

* **No personal-account login.** Discord has no supported interface that lets a third-party client
  read or send messages *as a user*. OAuth2 user scopes (`identify`, `guilds`, ...) give profile and
  server-list information only, not message access, and the local RPC interface is for the official
  client. Using a user token in a third-party client ("self-botting") breaks Discord's Terms of
  Service and Developer Policy, so Feather neither supports nor accepts it: after a token is entered
  the client calls `GET /users/@me` and refuses any account where `bot` is not true.
* No scraping, no imitation of the official client's requests, no rate-limit evasion, no reading of
  another application's stored tokens, no modification of Discord's files, no paid-feature unlocking.
* Feather must not claim personal-account support until a supported, permitted interface exists.

## What a bot account can and cannot see

| Capability | Status | Notes |
|---|---|---|
| Servers the bot has been invited to | Supported | `GUILD_CREATE` over the Gateway |
| Channel list, categories, threads | Supported | Visibility is computed locally from roles and overwrites exactly as in Discord's permission docs, so channels the bot can't view are hidden |
| Reading messages in servers | Supported with the **Message Content** privileged intent | Without it Discord blanks `content`, embeds and attachments except for DMs, mentions and the bot's own messages. Must be enabled in the Developer Portal; bots in 100+ servers need Discord's approval |
| Message history | Supported | `GET /channels/{id}/messages`, needs *Read Message History*; paged 50 at a time and cached locally |
| Sending, editing and deleting own messages, replies, attachments (10 MiB) | Supported | Needs the matching channel permissions |
| Deleting others' messages | Supported if the bot has *Manage Messages* | |
| Reactions (add and remove) | Supported | Needs *Add Reactions* |
| Real-time updates | Supported | Gateway events; no polling |
| Direct messages | **Partial** | A bot can only DM users who share a server with it, and only receives DMs people send to the bot. It does not see a person's own DMs |
| Group DMs, friends, call/voice, activities, stickers store, Nitro features | Not available to bots | |
| Member list | Supported with the **Server Members** privileged intent | Presence is not requested |
| User profiles and avatars | Supported | Avatars come from the CDN host allow-list |
| Unread markers and notifications | Supported (client-side) | Discord keeps no read state for bots, so Feather stores it locally. Notifications fire for mentions and, optionally, all DMs |
| Search | Local only | Bots have no server-side search; Feather searches the messages it has cached (SQLite FTS5) |
| Embeds | Supported (display) | Title, description, fields, image, colour |
| Custom emoji | Shown as `:name:` text | Images are not loaded yet |
| Voice, video, screen share | Not implemented | |

## Protocol obligations Feather implements

* Hello → jittered first heartbeat → regular heartbeats with the last sequence number; immediate
  reply to op 1; zombie detection when an ACK is missed (close with a non-1000/1001 code, then
  resume).
* Identify or Resume (op 6) using `resume_gateway_url`; op 7 and op 9 handling; identify spacing of
  5.5 s; fatal close codes (4004 auth failed, 4010-4014) stop reconnecting and tell the user why.
* Gateway sends stay under the 120 events / 60 s limit (110 reserved for commands, the rest for
  heartbeats) and the 4096-byte payload cap.
* Transport compression (`zlib-stream`) with one inflate context per connection.
* REST rate limits: per-route buckets from `X-RateLimit-*` headers (requests wait locally rather than
  being sent into an exhausted bucket), `Retry-After` and the global flag on 429, bounded retries,
  idempotent-only retry of 5xx. `User-Agent` follows the documented `DiscordBot (url, version)` form.
* The token is only ever sent to the configured API base and gateway, never to the image CDN.

## Verification status

| Claim | How it is checked | Status |
|---|---|---|
| Gateway lifecycle, resume replay, op 7/9, heartbeats, zlib framing | Automated tests against `feather-mock` | Verified against the mock only |
| REST paging, rate-limit handling, permission filtering | Unit and mock tests | Verified against the mock only |
| Behaviour with Discord's real Gateway/REST (field shapes, intents enforcement, rate-limit headers, CDN) | Needs a real bot token and network access to discord.com | **Unverified**: the build environment cannot reach discord.com |
| Windows and macOS keychain integration | Needs those platforms | **Unverified** (Linux Secret Service code path compiles; CI builds all three platforms) |

To verify against the real service, create a bot (see the README), run `feather`, and check:
sign-in, server and channel list, history, send/edit/delete, reaction, DM to the bot, reconnect
after toggling your network, and a 4014 message when an intent is not enabled.

## Where a future compliant personal-account option would plug in

All Discord I/O is behind `feather-core`: `rest::Rest`, `gateway::run` and the `engine` command/event
interface. The UI only sees `engine::Command` and `engine::Event`, plus the `Session` state. A new
transport that Discord explicitly permits could be added beside the bot transport without UI changes.
