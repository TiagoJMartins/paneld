# paneld — minimal TRMNL BYOS panel server

## Problem Statement

I drive an e-ink wall panel (a jailbroken Kindle Paperwhite 2 running KOReader's `trmnl-display` plugin) from a self-hosted dashboard server. The server I use today is a large, fast-moving all-in-one application: a Python web app with a plugin system, a browser-based dashboard editor, a marketplace, and a headless Chromium render pipeline. Three things about it do not fit how I work:

1. **I cannot push data to it.** There is no endpoint where any machine on my network can say "widget `slack_unread` now reads `on`". Its webhook replaces a whole dashboard or a whole image, and its structured-data bridge accepts only a closed set of three source names. Getting one boolean onto the panel from my Mac required patching four of the host's internal functions at import time, which is off-contract and breaks silently on upgrade.
2. **I cannot define a dashboard as a file.** Layouts live in a database behind a GUI editor. I want the panel described in version-controlled configuration, the way Home Assistant and gethomepage dashboards are.
3. **It is heavier than my needs.** A browser engine, a plugin loader and a widget marketplace exist to support every panel and every data source. I need weather from Home Assistant, a handful of pushed values, and speed of iteration.

There is also a concrete defect in what I run today: my Kindle is a 16-level greyscale panel, but the frames it is served are quantised to pure black and white, discarding 15 of those levels.

## Solution

A single static Rust binary that speaks the TRMNL BYOS protocol, renders dashboards described in a TOML file, and exposes one content endpoint that any device on my network can push to.

- The panel polls it over HTTP exactly as it polls the server it replaces, so switching over is a matter of repointing one base URL. Nothing is migrated.
- Dashboard layout is a TOML file. Widgets are declared with a grid position and a kind. There is no editor, no plugin system and no marketplace.
- Any device can `PUT` content addressed by widget id. Most recent write wins. No pairing, no per-device credentials.
- Frames are built on a schedule, not while the panel waits. A poll is a pure read of whatever frame is currently rendered, so a slow dashboard can never delay or fail a fetch. A push may optionally ask for an immediate rebuild.
- Rendering is browserless: layout and text are computed in-process and rasterised directly, then quantised to the panel's actual colour capability — 16-level greyscale for the Kindle, which is a visible improvement over the black-and-white frames it receives today.
- Panel capability (dimensions, palette, dither) is configuration, not code, so a mono, 4-colour or 6-colour panel is a config change rather than a new render path.

## User Stories

1. As a panel owner, I want the server to answer the TRMNL display poll with a valid frame, so that my existing Kindle displays a dashboard without any change to the device software.
2. As a panel owner, I want to point my device at a new base URL and have it work immediately, so that switching servers costs me one settings edit.
3. As a panel owner, I want to run one binary with one config file, so that I can deploy it without a runtime, a database or a browser.
4. As a panel owner, I want the server to serve my panel at its real resolution, so that frames are not letterboxed or scaled.
5. As a panel owner, I want frames rendered using all 16 grey levels my Kindle supports, so that text and charts look better than the 1-bit output I get today.
6. As a panel owner, I want the panel's colour capability declared in config, so that adding a mono, 4-colour or 6-colour panel later does not require code changes.
7. As a panel owner, I want to choose the dither algorithm per panel, so that I can trade tone accuracy against frame-to-frame stability.
8. As a dashboard author, I want to describe my dashboard in a TOML file, so that it lives in version control and I can diff and revert it.
9. As a dashboard author, I want to place widgets on a named grid with row/column spans, so that I control layout without writing code.
10. As a dashboard author, I want to reload the dashboard by editing the file, so that iterating on a layout does not require a rebuild.
11. As a dashboard author, I want a config error to be reported clearly and the previous good config to stay in effect, so that a typo never blanks my panel.
12. As a dashboard author, I want to preview a frame without a physical panel, so that I can iterate quickly at my desk.
13. As a script author on any machine, I want to push a value to a widget by id over HTTP, so that I do not need a plugin, a pairing flow or a client library.
14. As a script author, I want the most recent push to win, so that I never reason about merge semantics or ordering.
15. As a script author, I want pushing to be a single `curl` with a JSON body, so that a shell script, a Shortcut or a Home Assistant automation can all do it.
16. As a script author, I want to push several named values in one request, so that one widget can show a small group of related readings.
17. As a script author, I want a push to a widget id that no dashboard uses to be accepted and stored, so that I can wire up a publisher before I lay out its widget.
18. As a script author, I want to read back what is currently stored for a widget, so that I can debug a publisher without a panel in front of me.
19. As a panel owner, I want a widget whose publisher has gone quiet to say so on the panel, so that a dead script never reads as a confident value.
20. As a panel owner, I want to configure how long a widget's content stays fresh, so that a once-a-day publisher and a once-a-minute publisher can coexist.
21. As a panel owner, I want pushed content to survive a server restart, so that a redeploy does not blank the dashboard until every publisher happens to fire again.
22. As a panel owner, I want the panel to redraw only when the rendered frame actually changed, so that I do not burn e-ink refreshes or battery on identical output.
23. As a panel owner, I want to control the panel's poll interval from the server, so that cadence is a server-side decision.
24. As a panel owner, I want the server to never send a poll interval of zero, so that a bug cannot put the device into a hot loop and flatten its battery.
25. As a panel owner, I want to see when my panel last polled and what battery level it reported, so that I know it is alive and how it is doing.
26. As a panel owner, I want a poll for an unknown device id to return a legible placeholder naming the configured ids, so that a mistyped base URL is diagnosable on the panel itself.
27. As a panel owner, I want the server to identify my panel by the URL it polls, so that I do not have to manage tokens or MAC addresses.
28. As a panel owner, I want to run two panels off one server with different dashboards, so that I can add a second display without a second deployment.
29. As an operator, I want structured logs of each poll and each render, so that I can tell whether a blank panel is a poll problem, a render problem or a content problem.
30. As an operator, I want the render to be deterministic for the same inputs, so that a frame I preview is the frame the panel gets.
31. As an operator, I want the binary to embed its fonts, so that rendering does not depend on system font configuration.
32. As an operator, I want to build and run the project through the same task runner I use everywhere else, so that setup is one command.
33. As a Home Assistant user, I want a widget that reads an entity's state from Home Assistant, so that weather and sensor data need no bespoke publisher.
34. As a Home Assistant user, I want a Home Assistant fetch failure to degrade that widget only, so that one unreachable integration does not blank the whole dashboard.
35. As a panel owner, I want frames rendered on a schedule rather than while my panel waits, so that the poll is a fast read and a slow dashboard never delays or fails a fetch.
36. As a panel owner, I want every device to have a real frame ready before the server accepts its first poll, so that a restart never shows a placeholder on a working panel.
37. As a panel owner, I want to set how often each device's frame is rebuilt independently of how often the device polls, so that an expensive dashboard and a chatty panel are tuned separately.
38. As a script author, I want my push to be picked up by the next scheduled render without doing anything else, so that the simple case needs no extra parameter.
39. As a script author, I want to optionally request an immediate rebuild when I push, so that an important value is on the glass at the panel's very next poll even when scheduled rebuilds are infrequent.
40. As a script author, I want a push that requests a rebuild to return immediately, so that my script never blocks on rendering.
41. As a script author, I want requesting a rebuild for a widget no dashboard uses to be accepted rather than rejected, so that wiring a publisher up early stays painless.
42. As an operator, I want a burst of pushes to collapse into one rebuild per device, so that a chatty publisher cannot spin the renderer.

## Implementation Decisions

### Language, toolchain and task runner

Rust. The toolchain is managed by `mise`, whose `rust` plugin is a **core** plugin (confirmed: `mise registry` reports `rust  core:rust`, and `mise ls-remote rust` offers `stable`, `beta`, `nightly` alongside pinned releases such as `1.97.1`). Rust is not currently installed on the host, so the first `mise install` fetches it.

The project ships a `mise.toml` declaring `rust = "stable"` under `[tools]`, plus tasks wrapping the standard cargo invocations: a build task, a run task, a test task, and a task that renders a preview frame to a file. Tasks are thin wrappers over `cargo` — no build logic lives in `mise.toml`.

Build target is the host's native toolchain (glibc). A statically linked `musl` build is explicitly not part of this work; do not add a `musl` target, a cross-compilation task, or `.cargo/config.toml` linker configuration.

### Crate selection

Every version below was confirmed against crates.io during specification. Pin to these minor versions.

| Purpose | Crate | Version |
| --- | --- | --- |
| HTTP server | `axum` | 0.8 |
| Async runtime | `tokio` | 1.53 |
| Config + content deserialisation | `serde` | 1.0 |
| Config parsing | `toml` | 1.1 |
| Layout + text + rasterisation | `takumi` | 2.10 |
| Palette quantisation and dithering | `dithr` | 0.3 |
| PNG encoding | `png` | 0.18 |
| Content hashing | `sha2` | 0.11 |
| Error handling | `anyhow` | 1.0 |
| Logging | `tracing` + `tracing-subscriber` | 0.1 / 0.3 |

Three of these carry decisions that are easy to get wrong:

- **`takumi` is the layout engine, and it must not be replaced with an SVG renderer.** It wraps `taffy` (CSS block/flex/grid), `parley` (paragraph layout, shaping, bidi, UAX-14 line breaking) and `tiny-skia` (rasterisation), and it already constructs its font collection with system font discovery disabled — which is exactly the posture needed here. The obvious-looking alternative, rendering SVG with `resvg`, does not implement automatic text wrapping: SVG 1.1 has none, and SVG 2's `inline-size` / `shape-inside` are parsed by `usvg` but unimplemented. Choosing SVG means hand-computing every line break.
- **`png` is used directly, never through the `image` crate.** `image` rejects every sub-byte PNG bit depth and rejects indexed colour outright, which would silently cost the 4-bit packing that halves the frame payload. `png` supports Grayscale at 1/2/4/8/16 bpp and Indexed at 1/2/4/8 bpp, which covers every panel class in scope.
- **`takumi`'s own dithering is not the e-ink quantiser.** It has a `dithering` option, but it is anti-banding over RGBA against a 128-level virtual lattice. Palette reduction is a separate downstream stage using `dithr`, whose `QuantizeMode::gray_levels(n)` covers the greyscale panels and whose `QuantizeMode::Palette` covers mono, 4-colour and 6-colour panels with the same call.

Fonts are embedded in the binary with `include_bytes!` and registered with `takumi`'s font collection at startup. Use Inter for UI text and IBM Plex Mono for numeric readouts; both are SIL OFL 1.1 and redistributable inside a binary. Do not read fonts from the filesystem and do not enable any system-font feature.

### No code is copied from the server being replaced

The server this replaces is licensed AGPL-3.0. Do not copy, translate or adapt its source. This is a clean-room implementation written from the protocol behaviour described in this document. Reference implementations that *are* permissively licensed (the several MIT-licensed BYOS servers) may be read for ideas with attribution; two ideas worth taking from them are named under Further Notes.

### Device identity is the URL path, not a token

Devices are identified by a path prefix, not by an access token and not by a MAC address. Both client families build their request URL by plain string concatenation of a configured base URL and the endpoint path, with no normalisation, so a base URL of `http://host/d/kindle` yields `http://host/d/kindle/api/display` and the prefix survives to the server as route information.

Consequences that the implementation must honour:

- Routes are mounted under a per-device prefix: `/d/{device}/api/display`, `/d/{device}/api/setup`, `/d/{device}/api/log`.
- `{device}` selects a device's configuration block. An unknown `{device}` returns a placeholder frame, not an error, so a mistyped base URL is diagnosable on the panel itself.
- **Image URLs must be served under the same prefix** (`/d/{device}/frames/{hash}.png`). One client family attaches its auth headers only when the image URL string-prefixes its configured base URL; keeping frames under the prefix keeps that consistent.
- The access token is ignored entirely. Neither client validates it; one replays whatever string it was given and the other has the server mint it. There is no token store and no token comparison. If a token is ever needed because the server is exposed beyond the private network, it becomes an optional shared secret compared with `constant_time_eq`, and that is out of scope here.
- MAC addresses must not be used as identity. The KOReader client omits its MAC header entirely when auto-detection fails, on the reasoning that claiming a made-up MAC is worse than claiming none.

**Trailing slashes are a real hazard**: a base URL ending in `/` produces a doubled slash in the request path. The router must treat `/d/{device}/api/display` and `/d/{device}//api/display` as the same route, or the panel silently 404s.

### The TRMNL BYOS protocol contract

Three endpoints. The display poll is the only one that matters; the other two exist so that clients do not fail.

**`GET /d/{device}/api/display`** — the poll. Responds `200` with a JSON body:

```json
{
  "status": 0,
  "image_url": "http://host/d/kindle/frames/<hash>.png",
  "filename": "<hash>.png",
  "refresh_rate": 300,
  "update_firmware": false,
  "firmware_url": null,
  "reset_firmware": false,
  "special_function": "none"
}
```

Five hard rules, each of which breaks a real device if violated:

1. **Always answer HTTP 200, even for errors.** The firmware accepts only 200, 301 and 429; a well-formed 401, 404 or 500 is treated as a transport failure and the device backs off. Error conditions are signalled in the body's `status` field, never in the HTTP status line.
2. **`status: 0` means success.** Not `200`. A body status of `200` falls through the firmware's switch and the device does nothing at all.
3. **`refresh_rate` must always be a positive integer, in seconds.** A missing or zero value becomes a deep-sleep timer of zero, the device wakes instantly, and the battery is flat within hours. Clamp the configured value to a minimum of 30 seconds and a maximum of 86400 before serialising it, and treat an out-of-range config value as a config error rather than passing it through.
4. **`filename` is the device's cache key** — not the URL, not the bytes. If the filename is unchanged the device does not download at all; it repaints from its own flash. Therefore the filename must be content-addressed: the hex of a SHA-256 over the final encoded frame bytes, truncated to 16 bytes. Long names are folded by the client to the first 7 plus last 17 characters, so keep the stem short enough that the truncation cannot collide.
5. **The tail fields are safe constants forever.** `update_firmware: false`, `firmware_url: null`, `reset_firmware: false`, `special_function: "none"`. Never emit `reset_firmware: true` — it is a factory reset that wipes the device's credentials and WiFi configuration.

**`GET /d/{device}/api/setup`** — answered with constants: `{"status": 200, "api_key": "<any stable string>", "friendly_id": "<device name>", "image_url": "<a real PNG under the device prefix>", "message": "ok"}`. Note this endpoint uses an in-body status of `200`, unlike the display poll. Only one client family calls it, exactly once, and only after its base URL changes. `image_url` here is fetched unauthenticated and must be a real PNG or onboarding fails.

**`POST /d/{device}/api/log`** — accept any body, return `{"status": 200}` with HTTP 200. Log the body at debug level and discard it. This endpoint exists purely because one client family stops polling if it 404s. Do not parse it and do not persist it. Cap the stored log line length so a misbehaving client cannot fill the disk.

Additional protocol constraints on the frame itself:

- Serve PNG, not BMP. The BMP path in the firmware is rigid — it accepts only exactly 800×480, 1 bpp, a 48000-byte data section and a 2-entry palette — while the PNG path is flexible and decoded into whatever framebuffer the device has.
- Keep the encoded frame under **90 000 bytes**. That is a hard ceiling on the non-PSRAM boards; over it, the fetch fails.
- Do not gzip the frame response. The device explicitly requests identity encoding; a proxy that compresses anyway corrupts the fetch.
- Frames must be reachable *from the device*, so the configured public base URL must be a LAN or tailnet address, never `localhost` or a container-internal hostname.

### What the device reports, and the ceiling on it

The display poll carries device telemetry in request headers. Header lookup must be case-insensitive, and both spellings of each field must be accepted, because the two client families disagree:

| Datum | Headers |
| --- | --- |
| Battery percent | `percent-charged`, `battery-percent` |
| Battery voltage | `battery-voltage` |
| Signal strength | `rssi` |
| Firmware version | `fw-version`, falling back to `user-agent` |
| Panel dimensions | `png-width` / `png-height`, or `width` / `height` |
| MAC, model | `id`, `model` |

`battery-voltage` arrives as integer millivolts on some firmware and as decimal volts on others. Normalise: parse as a float, and if the value is below 100, treat it as volts and multiply by 1000. A real lithium cell never reads below 100 mV nor above about 5 V, which makes the heuristic safe.

Store the most recent reading per device in memory and expose it on a status endpoint. Merge partial readings rather than replacing wholesale: a field absent from the current poll must not erase a previously known value, or one header-light poll blanks the device's whole record.

Be aware of the ceiling this imposes, and do not build features that depend on data the panel does not send: the KOReader client sends battery as an integer percentage only (no voltage), hardcodes `rssi` to `"0"` as an unfinished TODO, and sends no firmware version and no model. Battery percentage is the only genuinely available statistic from the panel in service.

### Server-initiated push is impossible; do not design for it

The device deep-sleeps between polls with its radio off. The only wake sources are its timer and its physical button. There is no listening socket, no long poll and no subscription. **A push therefore becomes visible at the device's next poll, never sooner.** The only lever on that is returning a smaller `refresh_rate`, at a battery cost.

So the end-to-end latency from a push to the glass has two terms: the wait until the frame is rebuilt, then the wait until the device polls. A push with `render: false` waits up to `render_interval` for the first term; a push with `render: true` reduces that term to approximately zero. Neither affects the second term, which is bounded by `refresh_rate` and owned by the device. This is why `render: true` is a narrow optimisation rather than the default — it only helps when `render_interval` is longer than `refresh_rate`.

Do not attempt to shorten the second term with an out-of-band channel, and do not present the content endpoint as though it updates the panel immediately. Its contract is "the next frame will contain this".

One caveat worth recording because it makes server-side cadence advisory rather than authoritative on the panel in service: the KOReader client ignores the server's `refresh_rate` unless its own "use server refresh interval" setting is enabled, which is off by default.

### Dashboard configuration

A single TOML file, path given on the command line, defaulting to `paneld.toml` in the working directory. Read at startup and re-read when it changes on disk. **A parse or validation error leaves the previously loaded configuration in effect and logs the error**; it never takes the panel down. A parse error at startup, when there is no previous configuration, is fatal.

Structure — this is the contract, and a widget's placement is explicit rather than inferred from document order:

```toml
[server]
listen = "0.0.0.0:4444"
# Must be reachable from the device. Never localhost.
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"                 # selects the /d/<id>/ route prefix
width = 1024
height = 758
palette = "gray16"            # gray16 | gray4 | mono | bwry | spectra6
dither = "atkinson"           # atkinson | floyd-steinberg | bayer | none
refresh_rate = 300            # seconds, clamped to 30..=86400
render_interval = 300         # seconds, clamped to 5..=86400; defaults to refresh_rate
grid = { cols = 4, rows = 3 }

[[device.widget]]
id = "slack_unread"           # also the content push address
kind = "beacon"               # beacon | value | text | ha_entity
col = 0
row = 0
col_span = 1
row_span = 1
label = "Slack"
stale_after = 3600            # seconds; 0 disables the staleness timer

[[device.widget]]
id = "office_temp"
kind = "ha_entity"
col = 1
row = 0
col_span = 2
entity = "sensor.office_temperature"
label = "Office"
unit = "°C"
```

Widget kinds, and exactly what each renders:

- **`value`** — one pushed number or string as a large figure, with `label` as a caption and optional `unit`.
- **`beacon`** — a two-state indicator. `on_values` (a list of strings, defaulting to `["on", "true", "alert"]`) is matched against the pushed `state` field first and the `value` field second. Renders an icon and wording per state.
- **`text`** — a pushed string, wrapped to the cell.
- **`ha_entity`** — reads an entity's state from Home Assistant rather than from a push. Requires `[home_assistant]` config with `base_url` and `token`. A fetch failure renders that cell as unavailable and must not fail the frame.

Grid placement is resolved into a CSS grid container via `takumi`. Two widgets overlapping the same cell is a config validation error naming both widget ids. A widget whose span exceeds the grid bounds is a config validation error.

`refresh_rate` and `render_interval` are two independent clocks and must not be conflated. `refresh_rate` is advice sent to the device telling it when to poll again; `render_interval` is how often this server rebuilds that device's frame. They default to the same value because rendering more often than the device looks is wasted work, and rendering less often than it looks means it sometimes fetches a frame it already has. Lower `render_interval` only when you want a frame to be ready ahead of an unpredictable poll; raise it when a dashboard is expensive and you would rather push-trigger renders explicitly.

### The content endpoint

**`PUT /api/content/{widget_id}`** — accepts a JSON body, stores it, returns `200` with the stored record. Last write wins, unconditionally: no timestamp ordering, no conflict detection, no merge. This is deliberate; ordering semantics are the thing that made the previous approach hard to reason about.

Body shape:

```json
{ "value": "on", "state": "alert", "unit": null, "rows": null, "render": false }
```

`value` is required and may be a string, a number, a boolean or null. `state`, `unit` are optional strings. `rows` is an optional array of `{ id, label, value, unit, state }` objects for a widget showing a small group of related readings; when present, `value` is ignored.

**`render` is an optional boolean defaulting to `false`.** It controls whether this push provokes a render immediately or waits for the next scheduled one:

- **`render` absent or `false`** — the content is stored and nothing else happens. The next scheduled render for whichever devices use this widget picks it up. This is the default because it keeps a chatty publisher cheap, and because a device cannot see a new frame before its next poll anyway: when `render_interval` is at or below the device's `refresh_rate`, rendering sooner buys nothing at all.
- **`render` is `true`** — after storing, send every device id whose dashboard declares this widget id to the render loop's wake channel. Use this when `render_interval` is deliberately long but a particular value should be on the glass at the device's very next poll.

Two consequences to implement explicitly:

- `render: true` for a widget id that no device's dashboard declares is **not** an error. Store the content, log at debug that the render request matched no device, and return `200`. This keeps the accept-unknown-ids rule below intact.
- The response body is the stored record either way. It does **not** report whether a render happened, and it does not wait for one. `PUT` returns as soon as the content is stored and the wake message is queued; it never blocks on rendering.

Rules:

- **A push to an unknown `widget_id` is accepted and stored**, not rejected. Publishers are frequently wired up before their widget is laid out, and rejecting them makes that ordering painful.
- The server stamps `received_at` server-side. Client timestamps are not accepted, because the two publishers that matter cannot be trusted to have a correct clock and no ordering decision depends on it.
- Staleness is computed at render time from `received_at` against the widget's `stale_after`. A stale widget renders its label plus how long ago it was last seen, never its last value styled as current.
- Content is persisted so it survives a restart: write the whole store to a single JSON file, atomically (write to a temporary file in the same directory, then rename). Load it at startup. A corrupt or unreadable store logs a warning and starts empty rather than failing to boot.
- **`GET /api/content/{widget_id}`** returns the stored record, or `404` if nothing is stored. This is a debugging affordance; unlike the device endpoints, ordinary HTTP status codes are correct here because no firmware is reading it.
- Bound the store: cap the number of distinct widget ids and the byte length of each string field, so an errant publisher cannot exhaust memory or disk.

### The status endpoint

**`GET /api/status`** — a JSON object keyed by device id, for operators and for tests. Per device: `last_poll_at`, the merged telemetry record from the most recent poll, `frame_hash` of the frame currently being served, `last_render_at`, and `render_count` (a monotonic count of renders performed since process start).

`render_count` is not decoration: it is the observable that makes the render loop's behaviour testable without reaching inside it. Coalescing, startup rendering and the hash-unchanged path are all asserted through it. A render that produced an unchanged hash still increments it, because it did perform a render.

Ordinary HTTP status codes apply here; no firmware reads this endpoint.

### The render pipeline

**Rendering is driven by a schedule and by explicit request, never by a device poll.** A device poll is a pure read: look up the device's current frame record and serialise it. No rendering, no blocking, no timeout handling on that path. This keeps poll latency flat and independent of how expensive a dashboard is.

A single background task owns all rendering. It is the only thing in the process that renders, which removes every concurrency question: two frames for one device can never be produced at once, and the poll handler never contends with it.

The loop wakes on either of two events:

1. **Its per-device interval elapsing** (`render_interval`, below).
2. **A message on its wake channel**, carrying a device id.

On waking it drains the channel without blocking, deduplicates the device ids, unions them with any devices whose interval has elapsed, and renders each of those devices exactly once. Draining-and-deduplicating is what stops a burst of pushes causing a render per push.

Every configured device is rendered once at startup, before the listener starts accepting, so a device polling immediately gets a real frame rather than a placeholder.

Rendering one device, in order:

1. Resolve the dashboard: config grid plus, per widget, either its stored content record or its Home Assistant entity state.
2. Build a `takumi` node tree — a grid container with one child per widget, placed by the widget's row/column and spans.
3. Rasterise to an RGBA bitmap at the device's configured width and height.
4. Quantise to the device's palette with the device's dither algorithm, via `dithr`. `gray16` uses greyscale-level quantisation; `mono`, `bwry` and `spectra6` use a fixed palette. Work in linear light, not on gamma-encoded bytes, or the midtones come out wrong.
5. Encode with `png` at the narrowest bit depth the palette allows — 4-bit for `gray16`, 1-bit for `mono` — as paletted or greyscale output as appropriate.
6. Hash the encoded bytes: SHA-256, hex, truncated to 16 bytes. That hash is the frame's filename stem.

**Render unconditionally on every tick; let the hash decide whether anything changed.** Do not try to detect dirty content and skip the render — the render is cheap relative to a panel refresh, and skipping it is an easy source of a stale panel. Instead, compare the new hash with the current frame's hash:

- **Hash unchanged** — discard the newly encoded bytes and leave the frame record exactly as it was, filename included. The device's next poll sees the same `filename`, does not download, and does not repaint. This is the mechanism that satisfies rule 4 of the protocol contract, and it is why the hash must cover the encoded bytes rather than the inputs.
- **Hash changed** — the new frame becomes current, and the frame it replaced is retained and still served at its own URL, because a device may be mid-download of it. Retain exactly one generation back per device; anything older is dropped.

Choice of dither is a real operational decision, not cosmetic: error diffusion (`atkinson`, `floyd-steinberg`) gives better tone but makes pixels in unchanged regions differ between consecutive frames, which defeats the hash-comparison above whenever any part of the image changes; ordered (`bayer`) is stateless per pixel and therefore stable frame to frame. Default to `atkinson` for the greyscale Kindle, and document `bayer` as the choice if frame stability matters more.

### Placeholder and preview

- **Placeholder**: served when `{device}` is not a configured device id. Because every configured device is rendered at startup, a known device always has a real frame, so this is the mistyped-base-URL case and the placeholder should make that diagnosable: render the requested device id, the fact that it is unknown, and the list of configured ids, through the same quantise and encode path. Clamp placeholder dimensions to a sane maximum so a malformed request cannot allocate an enormous buffer.
- **Preview**: a CLI subcommand renders a named device once, writes the PNG to a path on disk, and exits without starting the listener or the render loop. This is the primary development loop and is exposed as a `mise` task.

## Testing Decisions

A good test here asserts externally observable behaviour — the bytes and JSON a client receives — and never reaches into internal structures. Tests must not assert on private helpers, struct fields, or the internal shape of the node tree, because all of those will be refactored; they must assert on what a device or a script sees.

Three seams. One would be preferable, but the product has three genuinely different observable surfaces: request/response JSON, pixels, and the render loop's scheduling decisions. Pixels cannot be usefully asserted through JSON, and wall-clock scheduling cannot be asserted deterministically through either.

**Seam 1 — the HTTP boundary (primary, and where most tests live).** Build the `axum` router over a config fixture and drive it in-process, without binding a port. The render loop's wake channel is exposed to the test so a render can be provoked deterministically instead of waiting on an interval.

- The display poll returns HTTP 200, and its body carries `status: 0`, a positive `refresh_rate`, and a `filename` matching its `image_url`.
- The display poll returns HTTP 200 even when the requested device does not exist, and its body still parses.
- A poll never renders: `render_count` on the status endpoint is identical before and after any number of polls.
- Every configured device has a frame before the first poll: immediately after startup, `render_count` is at least one per device and a poll returns a real frame rather than the placeholder.
- `refresh_rate` is clamped: configured values below 30 and above 86400 are rejected at config load, and the serialised value is never zero.
- The tail constants are exactly `update_firmware: false`, `firmware_url: null`, `reset_firmware: false`, `special_function: "none"`.
- **The filename changes only when the rendered bytes change.** Three cases, and this trio is the most valuable test in the suite because the whole e-ink refresh story rests on it: two consecutive polls with no render in between return the same `filename`; a render provoked with unchanged content leaves `filename` unchanged while incrementing `render_count`; a render provoked after a content push returns a different `filename`.
- A `PUT` with `render: false` (and with the field absent) does not change the served `filename` until a render is provoked.
- A `PUT` with `render: true` results in the new content being served without any interval elapsing.
- A `PUT` with `render: true` for a widget id no dashboard declares returns `200`, stores the content, and does not change any device's `filename`.
- A `PUT` with `render: true` returns before the render is observable, i.e. it does not block: the response arrives, and only afterwards does `render_count` rise.
- A burst of `PUT`s with `render: true` for widgets on the same device produces fewer renders than pushes — one render for the burst.
- The frame URL from a poll is fetchable and returns a PNG whose magic bytes are correct and whose length is under 90 000 bytes.
- The previous frame URL remains fetchable after a new frame replaces it, and the generation before that does not.
- The frame URL is under the device's path prefix.
- A doubled slash in the request path resolves to the same route.
- A `PUT` to a known widget id is stored and reflected by a subsequent `GET`; a `PUT` to an unknown widget id is also accepted and stored.
- The second of two `PUT`s to the same widget id wins.
- The setup endpoint returns in-body `status: 200` and an `image_url` that is fetchable and is a PNG.
- The log endpoint returns HTTP 200 and in-body `status: 200` for a well-formed body, a malformed body, and an empty body.
- Telemetry headers are parsed case-insensitively, both spellings are accepted, a voltage below 100 is interpreted as volts, and a poll missing a header does not erase a previously reported value.

**Seam 2 — the render pipeline (a pure function, config + content in, encoded PNG bytes out).** Pixels are the product, and asserting them through HTTP would make failures unreadable. Assert properties rather than golden images, because golden files over a fast-moving text stack are a maintenance tax:

- Output decodes as a PNG at exactly the configured width and height.
- A `gray16` panel yields at most 16 distinct pixel values; a `mono` panel yields at most 2.
- Encoded bit depth is 4 for `gray16` and 1 for `mono`.
- Rendering the same inputs twice yields byte-identical output. This is load-bearing, not hygiene: the filename-stability behaviour above is only correct if the encoder is deterministic.
- A widget whose content is older than its `stale_after` produces different bytes from the same widget when fresh.
- A widget with no stored content renders without error.
- A Home Assistant fetch failure renders the frame successfully with that one cell showing unavailable.

**Seam 3 — the due-device calculation (a pure function of the device set, their `render_interval`s, their last render times, and a supplied "now").** The render loop must not be tested against wall-clock time; extract the "which devices are due at instant T" decision as a pure function and test it directly:

- A device whose interval has elapsed is due; one whose interval has not is not.
- Two devices with different intervals become due independently.
- A device is not due twice for the same elapsed interval.
- `render_interval` defaults to `refresh_rate` when absent, and is clamped to 5..=86400.

Config validation is covered at the config-load seam, which is a plain function from TOML text to a validated config: overlapping widget cells, a span exceeding the grid, an out-of-range `refresh_rate`, an out-of-range `render_interval`, and a `public_base_url` pointing at localhost are each rejected with an error naming the offending widget id or field. A malformed config presented to a running server leaves the prior config in effect.

There is no prior art in this repository — it is new. Do not introduce a mocking framework; a config fixture plus a stub Home Assistant responder is sufficient, and the Home Assistant client should be behind a small trait so a stub can be supplied in tests without a network.

## Out of Scope

- Static `musl` linking and cross-compilation. Native host build only; no `musl` target, no linker configuration.
- Any GUI, web editor, or configuration UI. Configuration is the TOML file.
- A plugin system, a widget marketplace, or any dynamic loading.
- Authentication on any endpoint, and TLS. This serves a private network. Note that the device's own TLS does not validate certificates, so transport authentication would be theatre here regardless.
- Firmware OTA. The relevant response fields are hardcoded to their safe constants and never computed.
- Special functions, playlists, dashboard rotation, and multi-page cycling. One dashboard per device.
- Partial-region panel refresh. Every change is a full frame.
- Quiet hours and scheduling windows.
- Battery history, drain prediction, and charting. Only the most recent reading is kept.
- MQTT, and any transport other than the device's HTTP poll.
- Migrating anything from the existing server. Switching is repointing a base URL; there is no data to move.
- Any BMP output path.

## Further Notes

Two ideas worth taking from the permissively licensed reference implementations rather than reinventing: content-hash filenames as the entire cache-invalidation mechanism (hash the final frame bytes, use the hex as the filename stem, and the device's filename-based caching does the rest), and hardcoding the safe-constant response tail rather than modelling firmware fields that will never be used.

The most likely source of a silently blank panel, in order: a base URL with a trailing slash producing a doubled path segment; a frame URL that resolves for the server but not for the device; and a `filename` that did not change when the content did. The first two are addressed by explicit route handling and a validated `public_base_url`; the third is the subject of the highest-value test in the suite.

Because rendering is decoupled from polling, there is one failure mode that a poll-driven design cannot have: a render loop that has died or wedged while the HTTP listener keeps happily serving the last frame it produced. The panel then shows stale-but-plausible content indefinitely, which is worse than showing nothing. `last_render_at` and `render_count` on the status endpoint exist to make that visible, and the render task must log at error level and keep looping if a single device's render fails rather than letting the failure end the task.

On expectations for the panel in service: enabling "use server refresh interval" in the KOReader plugin's settings is required before the server's `refresh_rate` has any effect on that device. Until then the panel polls on its own configured interval and the server's cadence control is advisory.
