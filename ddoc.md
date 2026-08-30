# Design Document: "Clipline" — An Open-Source, Lightweight, Ad-Free Game Recorder for Windows

*A foundational engineering blueprint. Working codename "Clipline"; rename at will.*

## TL;DR
- **Build a native Rust core (windows-rs) for capture/encode/replay-buffer, paired with a Tauri (WebView2) UI, defaulting to Windows.Graphics.Capture with DXGI Desktop Duplication fallback and hardware encoders (NVENC/AMF/QuickSync) via dynamically-linked LGPL FFmpeg.** This yields a sub-15 MB installer, ~60–120 MB idle RAM, near-zero gameplay FPS impact, and — crucially — avoids DLL injection, the only architecture that is safe with Riot Vanguard, Easy Anti-Cheat, and BattlEye.
- **The differentiating feature — automatic timeline event markers — is built on official local game APIs, not injection and not the disliked Overwolf platform.** For League of Legends, poll the Live Client Data API at `https://127.0.0.1:2999/liveclientdata/eventdata`, which exposes 11 officially documented event types (ChampionKill, Multikill, Ace, DragonKill, BaronKill, TurretKilled, InhibKilled, HeraldKill, FirstBrick, MinionsSpawning, GameStart) with killer/victim/assist data and game-clock timestamps. For VALORANT, real-time events are far more restricted; rely on kill-feed OCR over our own captured frames, with optional post-match enrichment via the official VAL-MATCH-V1 API — noting that Riot production keys are secrets that cannot ship inside an open-source client, so enrichment is bring-your-own-key or proxied.
- **The project is sustainable without ads or telemetry**: GitHub Sponsors/donations, MIT/Apache-2.0 licensing for first-party code, and deliberate avoidance of GPL-encumbered libobs in favor of a thin custom pipeline. This precisely fills the gap left by Overwolf-based tools (Outplayed/Medal), heavyweight OBS, and vendor-locked ShadowPlay.

## Key Findings

### The competitive gap is real and well-defined
- **Outplayed.gg** lives entirely on the **Overwolf** platform, watermarks clips for guest (not-signed-in) use — a free account removes the watermark from clips; a subscription is needed only to customize/remove it on Editor-created projects — and uses Overwolf's Game Events Provider (GEP) for real-time event detection, which Overwolf's support docs say is enabled only for "select games" because it requires "ongoing maintenance _and_ permission from game developers." Users dislike Overwolf's resource overhead and ad-driven model.
- **Medal.tv** reported "more than four million gamers per month" and "over 1 million daily active users" in its July 11, 2024 PRNewswire release; the oft-cited "more than 2 million clips per day" figure dates to its December 2021 $60M raise. It works on all GPUs and auto-clips via "Automatic Event Detection" on Windows for ~15 named games (VALORANT, LoL, CS2, Fortnite, Rocket League, Dota 2, GTA V, PUBG, and more as of Nov 2025), but is cloud/social-oriented and its detection leans on each game's replay system and is sensitive to in-game "Streamer Mode."
- **NVIDIA ShadowPlay (NVIDIA App)** has the lowest overhead (dedicated NVENC, hooks the driver) but is NVIDIA-only, has weak editing, no sharing, and no event detection. **AMD ReLive/Adrenalin** and **Xbox Game Bar** are the equivalent vendor/OS tools — low overhead, no event tagging, no editor of note.
- **Steam Game Recording** (shipped 2024) is the closest analog to our differentiator: free, built into Steam, background replay buffer, and **timeline event markers via the Steam Timeline API**. But markers appear only in games that integrate that API — **League and VALORANT do not** — it covers Steam-launched games only, and its editor is minimal. It raises the bar for "free and lightweight" while leaving the Riot-titles event gap wide open.
- **OBS Studio** is the gold-standard open-source engine but is heavyweight and complex, and its Game Capture uses DLL injection that breaks against anti-cheat. Vanguard broke OBS 31.0 Game Capture via a signature change; OBS's own recommended workaround is Display/Window Capture (no injection).
- **gpu-screen-recorder** (Linux) validates the lightweight thesis: GPU-encoded (NVENC/VAAPI/AMF), RAM-or-disk replay buffer (`-replay-storage disk`, `-bm cbr`), ShadowPlay-like, minimal overhead — but Linux-only.
- **Powder / Eklipse** use cloud AI/computer vision for highlight detection (Powder: 40+ game-specific models plus audio/chat cues; Eklipse: 1000+ games) but are post-processing, not real-time. **Powder confirmed shutdown** — per its official "To the Powder community" notice (undated; public by late September 2025): "We've stretched our financial capacities as far as possible, but we can no longer sustain the app. Starting today, the Powder app will no longer be updated or maintained." This is a sustainability cautionary tale for a venture-funded model.

### Capture technology: avoid injection
- **Windows.Graphics.Capture (WGC)** shares GPU textures via DWM, works cross-GPU, supports HDR, and requires no injection — making it anti-cheat-safe. Requires Windows 10 1803+. **Default choice.**
- **DXGI Desktop Duplication** must run on the same GPU as the display and is limited to the compositor/monitor refresh rate; solid fallback that injects nothing.
- **Game Capture via DLL injection (OBS-style)** is the most efficient and supports exclusive fullscreen, but is functionally process injection that Vanguard/EAC/BattlEye block. This is disqualifying for VALORANT.

### Hardware encoding is essentially free
- NVENC (Ada) encodes a 1080p HEVC frame in ~1–3 ms at presets P1–P5 (per NVIDIA's SDK throughput tables) — sustaining 60 fps real-time is trivial at any preset. But "zero FPS impact" claims trace to anecdotal, methodology-free blog posts; methodical testing measures small-but-real costs (e.g., ExtremeBench: ~5% average FPS drop, range 2.9–9.7%, for ShadowPlay NVENC at 4K on an RTX 3090). Budget **~3–6% for NVENC/QSV capture**, slightly more for AMD AMF — cheap, not free.
- AV1 (RTX 40 / RX 7000 / Intel Arc, plus Meteor Lake+ iGPUs and RDNA 3 APUs): NVIDIA's technical blog reports "a 40% bit rate savings for AV1 over H.264 at 1080p60 at a similar quality," and separately that low-latency-preset gains of *up to* 40% represent "more than 1.8 GB of saved data for two hours of a 1080p 5 Mbps streamed video" — ideal for storage-efficient local recording.
- The NVENC concurrent-session limit on GeForce was raised 2→3 (2020), 3→5 (March 2023), 5→8 with Game Ready Driver 551.23 (Jan 24, 2024), and 8→12 via a silent support-matrix update circa November 2025. The current cap (12 per system, driver-imposed, combined across all non-qualified GPUs) is no longer a practical constraint.

### Riot event integration is viable for League, constrained for VALORANT
- **League Live Client Data API** (`127.0.0.1:2999`) is officially documented, requires no auth, opens during a match, and per Riot's Vanguard FAQ is **expected to keep working under Vanguard**. Verbatim from Riot's official "Vanguard FAQ for Third Party Applications": *"Apps developed using the LCU and in-game APIs are still expected to work. Please note the LCU and in-game APIs are not owned by Developer Relations and we cannot offer support or guarantee updates when using these methods."*
- **VALORANT** offers no personal API keys, a selective production-key approval process (mandatory RSO opt-in, up to three weeks to review), and its internal local/glz endpoints are unsupported. Real-time kill events are not reliably available without OCR.

## Details

### 1. Vision, Goals, Non-Goals

**Vision.** A genuinely lightweight, ad-free, telemetry-free, open-source game recorder for Windows that matches ShadowPlay's low overhead, beats Outplayed/Medal on privacy and resource use, and uniquely auto-annotates the recording timeline with real in-game events using official local APIs rather than the Overwolf platform.

**Goals (MVP):**
1. Instant replay buffer (retroactively save the last N seconds/minutes by hotkey), 30 s–20 min configurable.
2. Full-session recording + simultaneous replay buffer, selectable per game plugin or saved custom game.
3. Hardware-accelerated capture/encode with <5% gameplay FPS impact and <300 MB RAM while active (excluding large in-RAM replay buffers).
4. Anti-cheat-safe capture (no injection) — must work with VALORANT/Vanguard.
5. Built-in lightweight, lossless-where-possible clip trimmer/editor.
6. **Automatic event markers on the timeline for League of Legends** via the Live Client Data API.
7. No ads, no telemetry, no account required, no Overwolf.

**Non-Goals (initially):** streaming to Twitch/YouTube, full scene composition/compositor, cloud storage/social feed, macOS/Linux, mobile, AI/CV highlight detection (later phase), console capture.

**Target users:** competitive PC players (League, VALORANT, CS2, Apex) who want ShadowPlay-style replay without NVIDIA lock-in; privacy-conscious users who reject Overwolf/ads; content creators wanting fast local clips with event context; AMD/Intel GPU owners underserved by ShadowPlay.

**Visual customization.** Settings > General previews and persists seven first-party color
palettes: Booth (warm amber, default), neutral Dark and Light, Classic (midnight blue), Purple
(deep violet), Pink (deep rose), and OLED (true black). Alternate themes override the same semantic
CSS token set so
recording states and accessibility behavior stay consistent across palettes. Light also switches
native browser controls to their light color scheme. The in-app Clipline logo uses the same theme
selection while the packaged Windows icon remains the fixed product mark.

### 2. Competitive Analysis Matrix

| Tool | Platform | Overhead | Ads/Model | Replay buffer | Event detection | Editor | Anti-cheat safe | Open source |
|---|---|---|---|---|---|---|---|---|
| Outplayed.gg | Overwolf/Win | Medium-high | Ads + freemium (guest watermark) | Yes | Yes (Overwolf GEP) | Yes | Mixed | No |
| Medal.tv | Win/mobile | Low-med | Freemium, cloud, social | Yes | Yes (~15 games, replay/CV) | Yes (browser) | Mostly | No |
| ShadowPlay/NVIDIA App | NVIDIA only | Lowest | Free | Yes (Instant Replay) | No | Minimal | Yes (driver) | No |
| AMD ReLive/Adrenalin | AMD only | Low | Free | Yes | No | Minimal | Yes (driver) | No |
| Xbox Game Bar | Win | Low-med | Free | Yes | No | Minimal | Yes | No |
| Steam Game Recording | Win (Steam) | Low | Free | Yes | Yes (Steam Timeline API, opt-in games; not LoL/VALORANT) | Minimal (trim) | Yes (no injection) | No |
| OBS Studio | Cross | High/complex | Free | Yes (RAM buffer) | No | No | Game Capture breaks AC | Yes (GPLv2) |
| gpu-screen-recorder | Linux | Lowest | Free | Yes (RAM/disk) | No | No | N/A | Yes |
| Powder (defunct 2025)/Eklipse | Win/cloud | N/A (post) | Freemium AI | No (post) | Yes (AI/CV, 40–1000+ games) | Yes | N/A | No |
| **Clipline (proposed)** | **Win 10/11** | **Lowest tier** | **Donations only** | **Yes (RAM+disk)** | **Yes (official local APIs)** | **Yes (lossless trim)** | **Yes (no injection)** | **Yes (MIT/Apache)** |

### 3. System Architecture

```
                        ┌─────────────────────────────────────┐
                        │         UI Layer (Tauri/WebView2)     │
                        │  Library · Timeline+Markers · Editor  │
                        │  Settings · Hotkey config · Tray      │
                        └───────────────▲───────────────────────┘
                                        │ Tauri IPC (typed commands/events)
                        ┌───────────────┴───────────────────────┐
                        │        Core Service (Rust)             │
   ┌────────────┐       │ ┌────────────┐  ┌──────────────────┐  │
   │ Capture     │─tex──▶│ │ Encode      │  │ Replay Buffer    │  │
   │ Engine      │       │ │ Pipeline    │─▶│ Manager (ring)   │  │
   │ WGC / DXGI  │       │ │ NVENC/AMF/  │  │ RAM + disk seg.  │  │
   └────────────┘       │ │ QSV via     │  └────────┬─────────┘  │
   ┌────────────┐       │ │ FFmpeg/SDK  │           │            │
   │ Audio       │─pcm──▶│ └────────────┘  ┌────────▼─────────┐  │
   │ Capture     │       │                  │ Storage Manager  │  │
   │ WASAPI loop │       │                  │ Hybrid/frag MP4  │  │
   │ +per-app    │       │                  │ Quota safety lock │  │
   └────────────┘       │ ┌────────────────────────────────┐  │  │
                        │ │ Event Ingestion Service         │  │  │
                        │ │ ┌──────────┐ ┌──────────────┐   │  │  │
                        │ │ │ LoL       │ │ VALORANT      │   │  │  │
                        │ │ │ adapter   │ │ kill-feed OCR │   │  │  │
                        │ │ │ :2999     │ │ + post-match  │   │  │  │
                        │ │ └──────────┘ └──────────────┘   │  │  │
                        │ │ Generic: log parse · OCR · audio│  │  │
                        │ └─────────────┬──────────────────┘  │  │
                        │   Normalized events → timeline sync   │  │
                        └───────────────────────────────────────┘
```

**Components:**
- **Capture Engine:** WGC primary (HDR, cross-GPU, no injection); DXGI Desktop Duplication fallback; per-window or per-monitor. Frames stay as GPU textures to avoid CPU round-trips. **Exclusive-fullscreen games cannot be captured per-window without injection** — we recommend borderless fullscreen (OBS's own guidance for anti-cheat titles) and fall back to display capture otherwise, with a clear in-UI warning that display capture records everything on the monitor (notifications, overlays, alt-tabbed apps).
- **Encode Pipeline:** encoder abstraction probing hardware at startup by deterministic priority (H.264 first for compatibility, then backend merit NVENC → AMF → QuickSync → software; HEVC/AV1 remain explicit local-efficiency options). SDR desktop/game captures are normalized from WGC full-range RGB Rec.709 into limited-range NV12 Rec.709 and advertised in encoder/container metadata so players do not guess the wrong range. The LGPL-clean software tier is SVT-AV1 (with Microsoft's software H.264 MFT as the last resort); no GPL x264. CQP/VBR for quality-efficient local recording; CBR for replay-buffer predictability.
- **Replay Buffer Manager:** segment-based circular buffer of *encoded* video+audio in RAM, with disk-spill option. Keyframe-aligned segments so saved clips start cleanly.
- **Event Ingestion Service:** optional plugin-backed per-game event sources normalizing into a common event schema; synchronizes game-clock event times to recording timestamps.
- **Storage Manager:** Hybrid MP4 (fragmented internally for crash safety; finalized as standard MP4 on stop) with a disk-quota safety lock. By default saved clips are deleted only by an explicit user action; Settings can optionally auto-delete the oldest clips to make room.
- **Clip Editor:** keyframe-aligned stream-copy trim (lossless, instant) with optional boundary re-encode for frame accuracy; GIF/WebM export.
- **UI Layer:** Tauri + WebView2; timeline renders event markers; runs in the system tray.

### 4. Technology Stack Evaluation & Recommendation

**Core language: Rust (recommended) vs C++ vs C#.**
- **C#/.NET 8 + WinUI 3/WPF:** fastest UI iteration with CsWin32 bindings, but GC pauses and runtime footprint are liabilities for a real-time 24/7 capture loop, and the AOT story is immature for this workload.
- **C++:** maximal control and the native language of libobs/FFmpeg, but slower iteration and memory-safety risk in a long-running background service.
- **Rust (recommended):** memory safety for a 24/7 background recorder, excellent `windows-rs` bindings to WGC/DXGI/WASAPI/Media Foundation, mature FFmpeg bindings, and first-class Tauri integration. The existence of a `libobs-wrapper` crate and robmikh's `windows-rs` capture samples de-risks the native plumbing.

**Recording core: embed libobs vs custom pipeline.**
- libobs is powerful and proven and has a Rust wrapper, **but it is GPL-2.0-or-later** — adopting it forces the entire app to GPL and pulls in OBS's injection-based Game Capture, which we explicitly want to avoid. OBS developers have declined proposals to relicense the core to LGPL. **Recommendation: build a thin custom pipeline** (WGC/DXGI → encoder → muxer) so we can license permissively (MIT/Apache) and keep the binary tiny.

**UI: Tauri (recommended) vs Electron vs native WinUI/egui.**
- Electron: 80–150 MB installers, 150–300 MB RAM — contradicts the lightweight mandate.
- Tauri: WebView2-based, <10 MB installers, ~30–60 MB RAM idle (typical community-benchmark figures — they vary by app, and WebView2 is itself Chromium-based, so Windows memory wins are smaller than headlines suggest), Rust backend (same language as the core), built-in updater and code signing. Hoppscotch built its desktop app on Tauri (deliberately avoiding Electron) and markets it as "20x lighter than Insomnia and 15x lighter than Postman" in file size — vendor comparisons against Electron apps, but directionally consistent.
- egui/iced (pure Rust): even lighter, but slower to build a rich timeline/editor UI.
- **Recommendation: Tauri**, with the timeline/editor as a web canvas component — with two provisos. (1) **WebView2 cannot be relied on to decode our default codecs**: H.264 plays everywhere, but HEVC needs the paid OS codec extension and AV1 the optional AV1 Video Extension; editor preview therefore uses a native FFmpeg decode path feeding frames to the UI (§11), not an HTML `<video>` element. (2) To actually hit ShadowPlay-class idle RAM, the WebView2 view is **made inert when tray-hidden or taskbar-minimized**. The native lifecycle is revisioned (`Foreground` / `Tray` / `Taskbar`): after the host window is confirmed hidden, the frontend invalidates asynchronous work, releases playback and microphone Web Audio, disconnects poster work, and removes heavyweight gallery DOM; then the controller's `IsVisible` goes false and the view drops to `MemoryUsageTargetLevel::Low`. Restore returns to `Normal`, shows the controller, and performs one coalesced refresh. A snapshot returned by `frontend_ready` and revision-gap reconciliation cover autostart and missed-event races. Measured on a dev machine before the frontend teardown work, controller visibility plus `Low` trimmed the WebView2 resident set from ~230 MB to ~40 MB and the whole process tree from ~335 MB to ~155 MB while hidden; remeasure private working set and private commit after lifecycle changes rather than treating those historical figures as current proof. Scripts and network connections keep running, so the Save Replay hotkey still works — which is why this is preferred over `TrySuspend` (pauses scripts) and why `Low`/`Normal` and `Suspend`/`Resume` must not be mixed. **Fully destroying the webview and recreating the UI on demand is now the tray/close path** (slim-core 2026-08-04): cold `--autostart` uses `"create": false`, close-to-tray enters `Destroying`→`Destroyed` with queued open, and readiness is per window generation. Measured destroy/autostart private working set is **~71–92 MiB** with **zero** WebView2 children (product hard budget ≤120 MiB; ≤90 stretch). Taskbar minimize may still soft-hide + `Low` for restore latency. Absolute private commit remains telemetry (live recorder dominates), not the same absolute idle ceiling.

**Media: LGPL FFmpeg as a separate replaceable process (not linked). On-demand for the regular SKU.**
- Ship/build FFmpeg **without `--enable-gpl`/`--enable-nonfree`**. Default H.264 recording uses Media Foundation and does not require FFmpeg. Posters, SVT-AV1/FFmpeg backends, audio sidecar extract, and shareable clipboard export request the managed runtime on demand. Keep source offer + attribution for LGPL 2.1 §6; never link libavcodec into Clipline.
- Note that **codec patents are independent of license**: H.264/H.265 carry patent-pool obligations (HEVC's Access Advance pool alone covers 27,000+ patents, with a 25% rate increase for licensees joining after June 30, 2026 — deadline extended from the original Jan 2026). Prefer **AV1 + Opus** as the default: royalty-free by design under AOMedia/IETF terms, though not entirely pool-free — Sisvel runs an AV1 pool aimed at consumer hardware, and Dolby has asserted AV1 patents outside AOM's framework (Dolby v. Snap, 2026); exposure for software distribution is low but nonzero. H.264 remains available for edit compatibility.

**Installer/update:** Tauri signed updater with two GitHub channels. Nightly pulls from the rolling
`nightly` prerelease; an immutable `nightly-v<version>` tag on `develop` triggers that Windows
action. Stable pulls from GitHub `/releases/latest`; an immutable `v<version>` tag on `main`
triggers the matching action. Both test, build regular and standalone NSIS variants, verify pinned
runtime inputs, publish through a complete draft asset set, and redownload the public assets for
byte comparison. Settings → Updates chooses the channel; new installs default to Nightly.

### 5. The Game Event Marker System (Differentiating Feature)

**Design principle:** use only official, locally-exposed data the player can already see — never injection, never memory reading, never Overwolf. This keeps us anti-cheat-safe and policy-compliant. (Note: Overwolf's own GEP architecture runs game events "on a different thread, so game performance will not be hurt," and is one-way — but it requires the Overwolf runtime we are deliberately rejecting.)

**Normalized event schema (internal):**
```
Event {
  game_id: enum,            // LeagueOfLegends, Valorant, CS2, ...
  type: enum,               // Kill, Death, Assist, MultiKill, ObjectiveDragon, ...
  actor: string,            // killer / local player
  victim: string?,          // optional
  assisters: [string],
  subtype: string?,         // DragonType, TurretId, KillStreak count, etc.
  game_time_s: f64,         // seconds of game clock from source
  wall_clock: timestamp,    // computed
  recording_offset_s: f64,  // mapped onto the recording timeline
  importance: u8,           // for auto-clip thresholds later
}
```

**User bookmarks.** Not every moment worth finding again is one a game API reports, and full-session recordings are long. A **bookmark hotkey** (default `F7`) drops a user-placed marker at the moment it is pressed, anchored on the same `recording_t0` as game markers so both share one timeline. Bookmarks are deliberately *not* `GameEvent`s — there is no game identity to attribute one to while recording a custom game or nothing in particular — so they ride in their own `bookmarks` array in the `<clip>.markers.json` sidecar, and the review timeline shows them past the per-game marker filters. They work in both recording modes: in replays-only mode the rolling ring prunes bookmarks that scroll out of the window, exactly as it does game markers.

**Timeline synchronization.** The recorder timestamps each encoded frame against a monotonic clock at capture start (`t0`). Event sources report a game clock (e.g., League's `EventTime` in seconds since GameStart). The subagent confirmed there is no wall-clock timestamp in the League payload, so we anchor: sample current game time (`gamestats.gameTime`) and wall time together, then `recording_offset = (event.EventTime − current_gameTime) + (now − t0)`. The anchor is **re-sampled on every poll** rather than computed once, so game-clock pauses (custom/tournament pauses, remakes) and drift self-correct; markers already placed are never re-mapped. A small fixed latency offset (kill-feed/event-emit delay) nudges markers onto the visual moment.

#### 5a. League of Legends adapter (fully supported, MVP)
Poll `GET https://127.0.0.1:2999/liveclientdata/eventdata` (self-signed cert — trust Riot's `riotgames.pem` or pass `--insecure`-equivalent). Returns `{ "Events": [...] }`. Every event carries `EventID` (int, monotonic), `EventName` (string), `EventTime` (float, **seconds of game time**). Officially documented event types and their fields (verbatim from Riot's `liveclientdata_events.json`):

| EventName | Extra fields |
|---|---|
| GameStart | — |
| MinionsSpawning | — |
| FirstBrick | KillerName |
| TurretKilled | TurretKilled, KillerName, Assisters[] |
| InhibKilled | InhibKilled, KillerName, Assisters[] |
| DragonKill | DragonType, Stolen, KillerName, Assisters[] |
| HeraldKill | Stolen, KillerName, Assisters[] |
| BaronKill | Stolen, KillerName, Assisters[] |
| ChampionKill | VictimName, KillerName, Assisters[] |
| Multikill | KillerName, KillStreak (2–5) |
| Ace | Acer, AcingTeam |

Community-observed but **not in Riot's official sample**: `FirstBlood` (field `Recipient`) and `GameEnd` (field `Result`) — implement defensively and verify against live games before relying on them. We also pull `playerlist`/`activeplayer` to identify the local player and mark *their* kills/deaths distinctly. Poll cadence ~1–2 Hz; because `EventID` is monotonic, we de-dupe and only append new events.

**Queue tagging.** Once per detected match, Clipline derives the local League Client lockfile from
the captured game executable and reads `gameData.queue.id` from the authenticated loopback
`/lol-gameflow/v1/session` endpoint. Watching a client replay (`.rofl` on the game process command
line) is its own category and wins over that queue id, so a ranked replay is tagged Replay rather
than Ranked Solo/Duo. The raw ID, stable category, and friendly mode label are stored in the
existing session sidecar. The Library can therefore show and filter familiar types such as Ranked
Solo/Duo, Ranked Flex, Normal, ARAM, Arena, and Replay without a Riot login, remote API key, or
cloud lookup. Filter those types from Library search with a Discord-style `LoL Type:` token, not a
header dropdown. This is best-effort enrichment: unavailable or changed LCU data never interrupts
recording, and legacy/unenriched League sessions remain readable as an unknown game type.

**Per-game-type recording gate.** Settings carry a record toggle per queue category (including
Replay, plus an Unknown policy for failed lookups), defaulting to record-all. When any category is
off, a newly detected League game tears down the previous recorder immediately and defers its
replacement until one bounded lookup resolves the type: command-line `.rofl` detection first, then
LcuClient's 2 s LCU bound. A denied game is not recorded and emits a skip notice; manual session
recording always bypasses the gate. The verdict is cached per detection as Pending/Allowed/Denied
and governs every automatic start path, so settings saves or autostart resumes cannot start a
recorder for a pending or denied game; toggles changed mid-lookup are honored when the verdict is
computed.

#### 5b. VALORANT adapter (constrained)
Riot's Vanguard FAQ confirms in-game/LCU APIs "should continue to function" and that "overlays and internal tools using the API, game client, and in-game APIs should continue to function," but VALORANT has **no personal keys**, a selective production-key approval process, mandatory RSO opt-in, and undocumented/unsupported `glz`/local endpoints. Real-time kill events are not reliably exposed. **Additionally, a production API key is a secret: it cannot be embedded in an open-source, locally-distributed binary** — anyone could extract it from the client or read it in the repo. First-party post-match enrichment would therefore require a hosted key-holding proxy, which conflicts with the no-cloud-cost, local-first stance. **Strategy:**
1. **Kill-feed OCR (primary, real-time):** computer vision on the kill feed / spike timer captured from our *own* WGC frames — no game memory access, no API key, fully anti-cheat-safe, the same class of technique Powder/Medal use.
2. **Post-match enrichment (optional, opt-in):** use the official **VAL-MATCH-V1** API (RSO opt-in) to fetch round/kill data and retro-place markers by mapping round timings onto the recording. Because the key cannot ship in the client, this launches as **bring-your-own-key** for power users; a community-funded thin stateless proxy (no storage, no accounts) is a later option if demand justifies the hosting cost — an explicit, documented exception to "no cloud."
3. Never inject, never read memory.

#### 5c. Generic adapters (later phases)
- **Log-file parsing:** CS2 Game State Integration (the `-gamestateintegration` config file; note Dota 2 requires the same launch flag), VALORANT `ShooterGame.log`, Overwatch logs.
- **OCR/CV on kill feed:** detection for games without APIs. Powder trained 40+ per-game models plus emotion/chat-reaction layers; Medal leans on each game's replay system. Run on already-captured frames.
- **Audio-cue detection:** spikes/keywords (Powder added shouting/laughter detection) — lowest priority.

### 6. Replay Buffer Architecture
- **Games-only recording is opt-in.** Desktop/fallback capture remains the default. When Settings > Games > `Pause recorder when no game is open` is enabled alongside automatic game detection, the recorder stays armed but owns no capture/encode service while no enabled game is detected. The rail's separate Buffer control shows `Waiting`, Save Replay is disabled, opening an enabled game starts a fresh buffer, and leaving the game finalizes/stops that run without immediately reopening the fallback desktop source. An explicit full-session recording request wakes capture even while this policy is waiting.
- Continuously encode video+audio; push **encoded** GOP-aligned segments into a ring buffer sized by `max_replay_time × bitrate`. CBR mode (like gpu-screen-recorder's `-bm cbr`) gives predictable RAM/disk usage in high-motion scenes; CQP/VBR is the default for quality.
- **Retention is bounded by span *and* bytes, planned together.** Duration retention is exactly `replay_window_s`: across the existing ring plus the incoming segment, eviction keeps the latest keyframe at-or-before `incoming.end - replay_window_s`, so the surviving front is decodable and still covers the requested cutoff without a fixed time margin. The byte budget carries a 2× encoder-overshoot allowance as a hard cap rather than a duration target. When genuine bitrate overshoot requires more eviction than the duration bound, byte pressure may advance to the next keyframe (including the incoming keyframe) and **bytes win**: coverage can fall below the requested window because the alternative is unbounded growth. With no duration or byte pressure, a push does not realign or discard footage gratuitously.
- **RAM by default** (avoiding constant disk writes — a commonly cited community rationale; OBS's buffer is RAM-only by design, though OBS forum staff consider SSD wear a non-issue on modern drives), with **disk-spill** for long buffers (gpu-screen-recorder's `-replay-storage disk`). OBS estimates RAM and warns when it would overflow; we expose the same estimator.
- On **Save Replay** hotkey, flush from the oldest in-buffer keyframe to now into a Hybrid MP4. Memory-backed saves borrow the ring's encoded GOP payloads; disk-backed saves open one cache segment at a time and copy through a fixed 64 KiB buffer. Prefix audio selection is metadata-only, so neither mode duplicates or mutates the retained window. Provide a "don't re-clip overlapping footage" smart mode — OBS lacks this natively (users currently hack it with Advanced Scene Switcher macros that stop/restart the buffer and lose ~1 s between saves).
- ShadowPlay/Instant Replay parity: rolling N-minute window (NVIDIA's default save hotkey is Alt+F10), no perf hit due to the dedicated encoder.
- **Clocking & A/V sync:** every video frame (WGC `SystemRelativeTime`) and audio buffer (WASAPI position) is stamped against the same QPC timebase; the muxer derives PTS from these stamps rather than assuming a fixed frame cadence, keeping sync correct under VRR/G-Sync displays delivering irregular frame times and under transient frame drops. This timing layer is the single biggest hidden cost of skipping libobs and is treated as M0 core work, not polish.
- **Simultaneous full-session recording + replay buffer** (Goal 2) shares one encoder session per track: encoded GOP-aligned segments fan out to two sinks (ring buffer and a best-effort writer thread for the open recording file) rather than running duplicate encoder sessions — staying within session limits on older GPUs and keeping disk stalls off the capture loop. A full-session writer can be enabled per built-in game plugin or saved custom game in Settings > Games, or attached and finalized manually from the rail's Record control or either of two optional global Start / Stop recording shortcuts. A writer attached mid-stream begins at the next retained GOP origin and drops only audio samples whose timestamps precede that new file origin. Stopping a manual recording leaves the replay ring running. The rail reports these as distinct states: `Record`/`Rec` means a saved full-session writer is closed/open, while Buffer reports the replay service as Off, Waiting, or Ready. Full-session sink failures degrade to warnings and do not tear down replay capture.

### 7. Performance Budgets
- **GPU encode:** ≤3–5% GPU at 1080p60; near-zero with dedicated NVENC/QSV. (Caution: an RTX 3080 user reported OBS's replay buffer at ~27–28% GPU with look-ahead and psycho-visual tuning enabled, and still ~25% after disabling them — so we ship conservative low-latency presets with look-ahead **off** by default and treat encoder-feature creep as a real perf risk.)
- **Gameplay FPS impact:** target <5%; methodical tests measure ~3–6% with NVENC capture (claims of 0% are anecdotal), with AMD AMF historically slightly worse.
- **CPU:** <3% with hardware encode; avoid x264 — OBS staff guidance is that x264 is CPU-intensive and recommends hardware encoders to offload encoding entirely.
- **RAM:** UI+core idle ~60–120 MB; active RAM scales with the buffer (e.g., 1080p60 @ ~40 Mbps ≈ ~1.5 GB for 5 min) — expose the estimator and default to disk-spill beyond a configurable threshold.
- **Capture latency:** WGC GPU-texture-shared path with no CPU copy.

### 8. Anti-Cheat Safety Strategy (critical)
- **Never inject DLLs.** OBS Game Capture (injection) is blocked by Vanguard/EAC/BattlEye/FACEIT; security analyses note these systems "explicitly block process injection tools — and OBS's hook is technically a process injection." Vanguard broke OBS 31.0 Game Capture when OBS's new ECC code-signing certificate tripped Vanguard's signature-trust check (fixed in OBS 31.1 by dual-signing) — injection-based capture remains structurally fragile against anti-cheat.
- **Default to WGC** (DWM-level, no injection) → **DXGI fallback** (no injection). Both are exactly what OBS itself recommends for anti-cheat titles ("run the game in either windowed or borderless fullscreen and use Window Capture," or Display Capture, since it does not inject any DLL).
- For VALORANT specifically: WGC capture + kill-feed OCR on our own frames (+ optional post-match VAL-MATCH-V1 markers). Zero game-process interaction.
- Document clearly for users that Clipline uses no kernel driver, no injection, and no memory reading — and sign all binaries to reduce AV false-positives (the OBS hook DLL is routinely flagged because injection mimics malware).

### 9. Security & Privacy Stance
- **No telemetry.** Bounded first-party debug logs are always written locally so failures remain
  diagnosable, but nothing is exported automatically. A user may prepare, inspect, and explicitly
  confirm a sanitized private report from Settings > Support. Reports go only to the official
  operator-owned `clipline-support` intake, never Clipline Cloud or a self-hosted Cloud instance,
  and expire after 30 days.
- **No account required**; Riot RSO is used only if a user opts into VALORANT post-match enrichment.
- Event data is fetched only from local `127.0.0.1` endpoints; nothing leaves the machine without an explicit user action (e.g., manual upload/share).
- **Capture hygiene:** display capture records the whole monitor, so the UI warns when it is active, offers pause-on-focus-loss, and prefers per-window capture wherever the game allows it. Accidentally recording a password manager or DM popup into a clip is treated as a privacy bug, not a cosmetic one.
- **Elevated games require explicit consent.** Clipline normally remains `asInvoker`. If Windows reports that a detected game is elevated above Clipline, the warning may restart only Clipline's current executable through UAC for this launch; cancellation leaves the normal process alive, and the elevated child verifies the exact parent PID plus creation time before waiting for it to exit. The modal requires an explicit `Restart as Administrator` or `Not Now` choice rather than accepting backdrop clicks or Escape. The UI states that elevation resets the current replay buffer and never turns elevation into a saved startup preference.
- Fully open source so privacy claims are auditable — the structural opposite of Overwolf/Outplayed's closed, ad-driven model.

### 10. Storage Management
- **Container: Hybrid MP4** (OBS 30.2+ approach). During recording the file is a fragmented MP4 (resilient against BSOD/power-loss/disk-full because each `moof`/`mdat` fragment is independently decodable); on stop, a fast "soft remux" writes a full `moov` and overwrites the leading placeholder so the file appears as a standard, seekable MP4 — combining MKV-grade crash safety with MP4 compatibility. This defeats the classic MP4 moov-atom "total loss" failure. MKV is offered as an alternative for power users (record-MKV-then-remux is the long-standing safe workflow).
- **Rate control:** CQP/CRF ~18–22 default for recording quality; CBR for replay-buffer predictability.
- **Disk management:** configurable media folder, configurable quota, and per-game folders (gpu-screen-recorder demonstrates the save-script pattern). By default Clipline never deletes a non-empty saved or unfinished recording automatically. Before a replay save or full-session write would exceed the quota, recording stops and remains blocked until the user raises the quota, explicitly makes enough space, or enables auto-delete of oldest clips in Settings (which frees space before locking). Default to the system drive and warn about external-drive corruption risks (Outplayed documents that non-C: drives can cause corrupted/disconnected recordings). In-progress full sessions use a `.mp4.recording` suffix, count toward storage usage, and are recovered on the next launch; only zero-byte placeholders may be cleaned automatically.
- **Audio:** WASAPI loopback for system audio; **per-application loopback** via `ActivateAudioInterfaceAsync` with `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` and `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (documented for build 20348+/Win 11; works in practice on updated Win10 2004+ — see Caveats) to capture only the game process tree; separate mic track via WASAPI capture; **multi-track output** (game / mic / system) for editing.

### 11. Clip Editor
- **Lossless trim:** keyframe-aligned stream copy (instant, no quality loss) for cuts on GOP boundaries; **re-encode only the boundary GOPs** for frame-accurate trims.
- Timeline with event markers; in/out points; merge multiple replay clips into a montage (joining files, ShadowPlay-montage style).
- **Groups:** the trim action has a secondary **Add to group** path. Group membership and order
  live in each clip's existing `.clipline.json` sidecar, so the first member creates the group and
  deleting the last member removes it without a second database. Group members stay out of the
  top-level Library: one group card represents them with an asymmetric mosaic of up to four real
  clip posters. Local clip and group cards share a primary metadata order of duration, size, then
  relative modified time. Opening a group reuses the normal review player as a sequential playlist and repurposes
  the Match events rail for member posters/titles; rows are mouse-draggable with keyboard Up/Down
  fallbacks, and playback advances to the next member at end. Clipline disables Tauri's unused
  native file-drop interception so WebView drag events reach those rows. The next member is
  preloaded into a muted layered video that covers the main player's source swap until its first
  frame decodes, avoiding a blank visual boundary. Group mode reuses the standard review-header
  actions instead of a second toolbar: Explorer reveals the current member; Copy and Upload build
  the authoritative compilation; Delete confirms once before removing the group and its members.
  Right-clicking a rail member opens the app-owned Delete menu for that clip, and deleting the
  active member continues with a surviving neighbor. Export normalizes members to 1080p60 H.264/Opus and concatenates them into a
  normal editable local `compilation` clip; Upload creates that same compilation and hands it to
  the existing Clipline Cloud title/description/visibility dialog. Each member's enabled embedded
  audio streams are normalized and mixed before concatenation, so split Output + Microphone clips
  keep both sources. The group header resolves the latest group compilation through normal local
  cloud records, so a successful public/unlisted upload changes Upload into Copy cloud link just as
  it does for an ordinary clip. V1 compiles at most 64 members
  per group and does not include group rename or moving an already-existing library clip between
  groups.
- **Export:** MP4 (H.264 for compatibility, AV1/HEVC for size), plus **GIF/WebM** for sharing.
- A successful trim export adds the result to the local library immediately and exposes an **Open clip** action beside the success status, avoiding a back-to-library search. The review stage supports standard fullscreen playback through its transport control or `F`; `Esc` returns from fullscreen before the existing close-clip shortcut applies.
- **Preview decodes natively** (FFmpeg + D3D11VA hardware decode), presenting frames to the UI as shared textures — WebView2's `<video>` cannot be assumed to play AV1/HEVC (§4), and frame-accurate scrubbing of high-bitrate streams needs our own decode loop regardless.

### 12. Risks & Mitigations
| Risk | Mitigation |
|---|---|
| Riot changes/removes local APIs (officially unsupported) | Adapters are modular; fall back to kill-feed OCR + post-match data; community can patch quickly after game patches |
| WGC perf worse without HDR/HAGS on some setups (one OBS user reported "terrible" WGC perf without HDR) | Auto-benchmark at first run; let users pick WGC/DXGI; ship sane presets |
| Anti-cheat false positives | No injection/driver/memory access at all; document, sign binaries, engage AV vendors |
| FFmpeg/codec patent exposure | Default AV1+Opus (royalty-free by design; Sisvel's AV1 pool and Dolby's assertions are a watch item); H.264/HEVC via vendor hardware encoders (license typically conveyed by GPU/OS); LGPL dynamic-link compliance |
| VALORANT key constraints (no personal keys; a production key cannot ship in an open-source client) | Ship LoL markers first; VALORANT via kill-feed OCR needing no key; post-match enrichment as bring-your-own-key, with a community proxy only if separately funded |
| Steam Game Recording covers the free/lightweight ground | Differentiate on event markers for games that don't integrate the Steam Timeline API (League, VALORANT) and on working outside Steam |
| Scope creep toward OBS-like complexity | Strict MVP; enforce non-goals |
| Single-maintainer sustainability (Powder died of funding) | Permissive license to grow contributors; donations/sponsors; no cloud cost burden |

### 13. Open-Source Licensing
- **First-party code: MIT or Apache-2.0** (permissive, contributor-friendly — the opposite of OBS's GPL copyleft).
- **Avoid libobs (GPL-2.0-or-later)** to prevent forced copyleft and to exclude its injection-based capture.
- **gpu-screen-recorder is GPL-3.0-only**: treat it as design validation and reference only — no code may be copied. The same diligence applied to libobs applies here.
- **FFmpeg: LGPL dynamic-linking** — no `--enable-gpl`, no statically-linked x264/x265; provide source, build steps, attribution, and a means to replace the library (LGPL 2.1 §6).
- Prefer **AV1/Opus** as the royalty-free-by-design default (see §4 for the Sisvel/Dolby caveat); use OS/GPU-provided H.264/HEVC encoders where needed.

### 14. Monetization-Free Sustainability
- **GitHub Sponsors + Open Collective donations**; optional one-time "supporter" cosmetic with **no feature gating and no watermark ever** — explicitly the anti-Outplayed stance.
- No ads, no cloud costs to recoup (local-first), keeping the financial burden minimal and avoiding Powder's fate.
- Optional self-hosted/community sharing later, never mandatory.

### 15. Roadmap / Milestones
- **M0 (Foundation):** Rust core, WGC capture, NVENC/AMF/QSV encode, Hybrid MP4 writer, Tauri shell, hotkeys, tray.
- **M1 (MVP):** Replay buffer (RAM+disk), full recording, multi-track audio (system + per-app + mic), library UI, lossless trim editor (incl. the native preview-decode path), settings.
- **M2 (Differentiator):** League Live Client Data adapter + timeline event markers + auto-clip-on-event option.
- **M3 (Breadth):** VALORANT kill-feed OCR adapter; optional VAL-MATCH-V1 post-match enrichment (bring-your-own-key + RSO); CS2 GSI log adapter; DXGI fallback hardening; HDR.
- **M4 (Polish):** OCR/CV generic event detection; GIF/WebM export; montage builder; auto-update; AV1 default.

### 16. Success Metrics
- <5% gameplay FPS impact verified across NVIDIA/AMD/Intel.
- Regular installer ≤25 MB setup (design stretch toward <15–20 MB uncompressed intent) **without** embedded FFmpeg; standalone/Fixed-Version SKU remains large/offline. Idle private working set ≤120 MB hard (≤90 stretch) with zero WebView2 children while trayed/autostarted. LGPL FFmpeg is an on-demand managed runtime under `%LOCALAPPDATA%\\Clipline\\ffmpeg`, not Core installer bytes.
- Works in VALORANT with zero anti-cheat issues (zero injection).
- ≥90% of League ChampionKill/Multikill/objective events correctly marked within ±1 s on the timeline.
- GitHub stars/contributors, Sponsor count, and clip-saves per active user as adoption signals — measured only via opt-in or aggregate, never silent telemetry.

## Recommendations
1. **Commit to the no-injection architecture now** (WGC default, DXGI fallback). This is the single most important decision: it is the only path that works with Vanguard-protected VALORANT and avoids the anti-cheat breakage that plagues OBS Game Capture.
2. **Choose Rust + Tauri + dynamically-linked LGPL FFmpeg, and do NOT embed libobs.** This protects the permissive license, hits the lightweight footprint targets, and unifies the language across core and UI.
3. **Ship League event markers as the flagship in M2** using the documented `:2999` Live Client Data API — lowest risk, highest differentiation, and explicitly allowed under Vanguard.
4. **Plan around VALORANT's API constraints rather than against them**: kill-feed OCR is the primary VALORANT path (real-time, no key); post-match VAL-MATCH-V1 enrichment is an opt-in, bring-your-own-key feature. A Riot production key cannot ship inside an open-source client, so a hosted key proxy is a deliberate, separately-funded decision later — not a default dependency.
5. **Default to AV1+Opus** for royalty-free, storage-efficient recording, with H.264 for compatibility.
6. **Benchmarks that change the plan:** if WGC shows >5% FPS impact on common configs, gate it behind auto-detected HAGS/HDR and default those setups to DXGI; if RAM-buffer estimates exceed a user threshold, auto-switch to disk-spill; if the AMD AMF path exceeds the 5% budget, prefer it only when no other encoder is present.

## Caveats
- The Live Client Data and LCU APIs are **officially unsupported** by Riot (no uptime/change guarantees) even though they are allowed — expect occasional breakage after patches.
- `FirstBlood` and `GameEnd` events are community-observed and not in Riot's official sample; verify against live games before depending on them. `DragonType` values beyond "Earth"/"Elder" (Fire/Infernal, Ocean, Cloud, Chemtech, Hextech) are not enumerated in Riot's sample and need live confirmation.
- Widely circulated "zero FPS impact" encoder figures trace to anecdotal, methodology-free blog posts; methodical testing shows ~3–6% capture cost on NVENC and slightly more on AMD. Treat all such figures as directional and re-benchmark on target hardware.
- Hardware AV1 encode requires recent silicon (RTX 40 / RX 7000 / Intel Arc, plus Meteor Lake+ iGPUs and RDNA 3 APUs) — probe encoder capabilities at runtime rather than gating on GPU model; older hardware falls back to HEVC/H.264.
- Codec patent licensing for H.264/HEVC is a legal question independent of FFmpeg's license; relying on GPU/OS-provided encoders typically conveys the license, but confirm for redistribution.
- Per-application audio loopback is *documented* for Windows 10 build 20348+ / Windows 11, but in practice works on fully updated Windows 10 2004+ (19041+) — OBS 28+'s Application Audio Capture relies on exactly this API there. Treat Win10 support as undocumented/best-effort and fall back to full-system loopback on failure. The process-loopback path's `GetMixFormat`/`IsFormatSupported` return `E_NOTIMPL`, so a fixed capture format (e.g., 48 kHz/16-bit stereo) must be assumed.
- Medal's and Powder's "AI" detection details are partly self-reported marketing; our OCR/CV plans should be validated empirically rather than assumed equivalent.
- WGC draws a yellow capture border that is only suppressible (`GraphicsCaptureSession.IsBorderRequired`) on Windows 10 build 20348+/Windows 11; older builds will show it — a visible difference from ShadowPlay worth documenting up front.
- Exclusive-fullscreen titles force display capture (no per-window WGC without injection); borderless fullscreen is the recommended mode, matching OBS's anti-cheat guidance.
- SDR color must stay explicit across conversion, encode, and muxing: WGC frames are treated as full-range RGB Rec.709, converted to limited-range NV12 Rec.709, tagged on MFT/FFmpeg encoder boundaries, and written as MP4 `colr`/`nclx`. HDR capture, ICC profile handling, and blue-light-filter interactions remain separate validation work.
- **FFmpeg encoder tier is a subprocess, not a link** (§4): Clipline drives a bundled `ffmpeg.exe` (raw NV12 piped in, elementary stream framed back out) rather than linking libavcodec — no unsafe FFI, version-robust, and the cleanest LGPL boundary (FFmpeg stays a separate program). Ship a **win64 lgpl-shared** build (e.g. BtbN): it contains SVT-AV1 and the GPU vendor encoders (NVENC/AMF/QSV) but **no `libx264`/`libx265`** (those are GPL), so there is **no FFmpeg software H.264 and no software HEVC** — the FFmpeg software tier is SVT-AV1 (AV1), while Microsoft's inbox software H.264 MFT is the native last resort. Discovery order is an explicit `CLIPLINE_FFMPEG` override → packaged resource → executable directory → the installed `%LOCALAPPDATA%\Clipline\ffmpeg` runtime → the legacy `%APPDATA%\Clipline\ffmpeg` development bundle → PATH. Release bundling verifies the exact staged allowlist, hashes, provenance, version, and LGPL configuration offline. Gallery poster extraction pipes a bounded JPEG back to Clipline so the FFmpeg child never needs write access to protected recording folders. The probe parses `ffmpeg -encoders` and test-encodes each hardware encoder at 640×360 (AMF rejects tiny resolutions), so a compiled-but-unusable encoder is dropped. SVT-AV1 takes only `-b:v` + `-preset` — it errors on `-maxrate/-bufsize`, so CBR capping is hardware-only.
- **WebView2 cannot decode HEVC/AV1 without OS codec extensions** (§4/§11): the review player probes `canPlayType` at startup; Automatic recording is restricted to decodable codecs (H.264 always), and explicit HEVC/AV1 picks carry a "limited playback" caveat. A native FFmpeg decode path for the editor (so any codec previews in-app) remains a separate milestone.
- Steam Game Recording (2024) overlaps the free/lightweight positioning; our durable differentiation is event markers for non-Timeline-API games (League, VALORANT), the local-first privacy stance, and working outside Steam.
