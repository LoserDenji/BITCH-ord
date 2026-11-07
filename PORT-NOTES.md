# PORT-NOTES — BitChord features on limusic

This tree is limusic (github.com/SimoHypers/limusic) with four features ported from the BitChord
desktop player: the **lyrics animations** (the am-lyrics "wipe" language), the **glass player bar**,
the **Dynamic Island**, and a **Listen Together server toggle** that can join BitChord party rooms.
Everything else — libmpv playback, innertube, library, scrobbling, theming — is limusic's own,
untouched.

Every change is additive: no limusic behavior changes unless you switch one of the new settings on.

---

## 1. Lyrics — the BitChord wipe

BitChord's lyrics look is the `@uimaxbai/am-lyrics` web component fed a TTML + a steered clock.
Per the port brief ("same animation, almost the same performance as limusic") the **visual language
was reimplemented natively** instead of shipping the web component: same wipe, rise, unblur and
dim, riding limusic's existing 4 Hz position ticks + rAF interpolation clock, so there is no extra
loop, no extra DOM pass, and no second lyrics engine to maintain. limusic's own bounce/float/wave/
pop/slide animations are all still there.

How it renders, when `Word animation = BitChord wipe` is picked (Appearance ▸ Player view, or
Playback ▸ Lyrics):

- Word fill: the hard-stop karaoke gradient (`lit p% / dim p%`) sweeping per word — the same
  two-stop gradient the stock sweep already draws, so the per-frame cost is one CSS property.
- Word motion: none. The wipe never moves words; its motion is the fill.
- Line rise: the active line steps forward (`scale 1.04`), the stock behavior.
- Unblur: lines ahead of the active one are blurred/dimmed by distance (≤3 lines → 2px, ≤8 →
  3.5px, further → 5px) and ease back as they approach — pure `filter`/`opacity` transitions.
- Sung lines dim to 0.45/0.38 (light/dark), slightly deeper than the stock 0.55/0.40.
- Line-timed lyrics get estimated word timing (the same synthesis "also animate synced lyrics"
  uses), so a line-only source still fills word by word, as am-lyrics did.
- The lyrics typeface follows the new `--font-lyrics` token (defaults to the theme's heading face;
  the BitChord theme points it at SF Pro Display).

| File | Change |
| --- | --- |
| `ui/src/lib/components/LyricsView.svelte` | `wipe` derived + `lineTone()`/`wipeBlurClass()` (line falloff), sweep snippet uses the wipe fill, `getWordTransform` returns identity for wipe, `needsFrameClock` + `getLineWords` include wipe, line transition now carries `filter`, lines use `--font-lyrics` |
| `ui/src/lib/theme.svelte.ts` | `'wipe'` added to `LyricsAnim` + `LYRICS_ANIMS` (settings picker picks it up automatically) |
| `ui/src/routes/layout.css` | `--font-lyrics` token (default → `--font-heading`) |
| `ui/src/lib/locales/en.json` | `settings.themes.lyrics_anim_wipe` |

## 2. Player bar — the BitChord glass pill

A Svelte rewrite of BitChord's `PlayerBar.tsx` / `Lcd.tsx` / `useScrubber.ts` / `VolumeControl.tsx`
wired to limusic's `playback` store and `api` commands. Selected in Settings ▸ Playback ▸
Interface ▸ **Player bar style** (`Limusic` / `BitChord glass`); limusic's own bar is untouched
and remains the default.

What carried over from BitChord: the 3-column pill (transport | LCD | actions), the MusicKit
transport glyphs, the LCD with artwork-expand-on-hover, title→album link, the 2px→4px hover seek
line with elapsed/remaining riding its ends, the names blurring under the scrubber on hover,
favourite / lyrics / queue / comments actions, the slide-in volume with the Apple speaker glyph and
wheel volume (non-passive listener, 5% steps), and the no-reactive-state scrubber loop — one
rAF-scheduled writer putting `--progress` on the fill and text into the time spans, 10 Hz idle,
frame-rate while dragging, nothing when hidden.

Adapted for limusic: tempo-aware display (labels divide by `playback.speed`, the fill fraction is
tempo-invariant), volume 0–100 with limusic's `dragVolume`/`commitVolume`/`toggleMute`/echo guard,
transport through `api.*` commands, repeat uses `cycleRepeat()`, queue/comments/lyrics buttons
carry the same props limusic's bar gets from the layout.

| File | Change |
| --- | --- |
| `ui/src/lib/components/BitChordBar.svelte` | **new** — the pill |
| `ui/src/routes/+layout.svelte` | renders `BitChordBar` or `PlayerBar` off `prefs.playerStyle` |
| `ui/src/lib/player.svelte.ts` | `prefs.playerStyle` + `setPlayerStyle()` + hydration in `initApp` |
| `ui/src/lib/components/SettingsDialog.svelte` | Interface section + segmented picker |
| `ui/src/lib/locales/en.json` | `settings.interface.*` |
| `src-tauri/src/commands.rs` | `player_style` added to `UI_SETTINGS` |

## 3. Dynamic Island

A near-verbatim port of BitChord's island (`island.html` + `src/island/main.ts` + `island.css` +
`island.rs`), re-hosted on limusic's architecture. The structural difference: BitChord's shell
pushed a media snapshot into the island page because its webview owned playback; limusic's backend
owns playback and already broadcasts `position` (4 Hz), `now-playing`, `playback-state` and
`playback-params` to every window, so the island page subscribes to the same store the mini player
uses and interpolates the position itself (10 Hz draws, frame-rate drags). No Rust sync loop at
all.

Behavior is the original's, unchanged: mini pill (216×44) ↔ expanded card (432×188) morphing on a
click, auto-collapse after 5 s, tape-crawl for long titles/artists, scrub-to-seek on release,
right-click dismissal until you've been back in the app and left again, click-through while
hidden, spawn pop-in, reduced-motion support, and platform-atomic window geometry (macOS
`setFrame:display:`, Linux `gdk_window_move_resize`, Windows one `SetWindowPos`) so the morph
never reads in two steps. Transport presses go through `island_action` → the same AppState calls
the tray menu items make. The artwork/placeholder mark is a neutral note glyph (BitChord's logo
stayed behind).

Toggle: Settings ▸ General ▸ System ▸ **Dynamic island** (`island_enabled`, ships on). The window
is built/destroyed the moment the switch flips.

| File | Change |
| --- | --- |
| `src-tauri/src/island.rs` | **new** — window creation, atomic geometry, click-through, focus push, unit tests |
| `src-tauri/src/commands.rs` | **new commands** `open_island`, `island_action`, `island_focus_state`, `island_resize`, `island_overlay`; `island_enabled` in `UI_SETTINGS` + `set_setting` side effect |
| `src-tauri/src/lib.rs` | `mod island`; `island::init` in setup; command registration; `CloseRequested` + `Focused` window-event arms; `only_ui` allows the island label (+ test updated) |
| `src-tauri/src/state.rs` | `is_paused()` accessor (the island resolves explicit play/pause against the same flag MPRIS reads) |
| `src-tauri/capabilities/default.json` | `"island"` added to `windows` |
| `src-tauri/Cargo.toml` | `objc2-foundation 0.3` (macOS only — the atomic `setFrame:`) |
| `ui/src/lib/components/Island.svelte` | **new** — the card + the brain (store-driven port of `main.ts`) |
| `ui/src/lib/components/island.css` | **new** — the card's recipe, glass tokens mapped to limusic's accent |
| `ui/src/routes/+layout.svelte` | `isIsland` label branch → `<Island/>` + slim `initApp(true)` boot |

## 4. Listen Together — BitChord server toggle

New in the Listen Together dialog's server panel: a **Party server** pick (`Limusic` /
`BitChord`). Limusic rooms speak the built-in relay protocol exactly as before; BitChord rooms
speak the BitChord party protocol (REST membership + WS with `bitchord.v1` +
`bitchord.token.<token>` subprotocols + `control` frames), so limusic can host and join the same
rooms BitChord's desktop/web players do. Invites carry the kind: a BitChord room mints
`CODE@bitchord` (or `CODE@bitchord:https://custom-server`); parsing, hosting and joining all
understand it, and the legacy `LMSC~` format still parses.

Mapping notes (documented in `bitchord.rs`'s header): play/pause/seek/track/queue map onto
BitChord's `control` frames — every member publishes and the server enforces `hostOnlyControl`,
so an unlocked BitChord room lets limusic guests drive (limusic's strict guest lock only applies
to its own relay and to host-locked BitChord rooms). Position sync uses the server anchor with
NTP-style clock correction (best-of-smallest-RTT pong) and drift correction past 1.2 s.
Suggestions become `queueAdd` (BitChord has no approval step); kick maps directly; host
transfer is not in BitChord's protocol, so it notices instead. Identity is a per-process random
id — BitChord hashes the signed-in YouTube account, which limusic doesn't expose to the session;
it carries nothing sensitive and just needs to be stable for the session.

| File | Change |
| --- | --- |
| `src-tauri/src/listentogether/bitchord.rs` | **new** — the BitChord protocol client (REST, socket, frames, clock, control mapping) |
| `src-tauri/src/listentogether/mod.rs` | `ServerKind`, BitChord fields on `Inner`, dispatch in `create_room`/`join_room`/`broadcast_playback`/`kick`/`suggest`/`leave`/guest gating, `serverKind` in the snapshot, `connect_ws_protocols` |
| `src-tauri/src/commands.rs` | **new command** `lt_set_server_kind` |
| `src-tauri/src/lib.rs` | kind read from `lt_server_kind` at startup + registration |
| `ui/src/lib/api.ts` | `LtState.serverKind` + `ltSetServerKind()` |
| `ui/src/lib/lt.svelte.ts` | `serverKind` default |
| `ui/src/lib/components/ListenTogether.svelte` | server-kind picker, `CODE@bitchord[:server]` invite mint/parse, URL reset on switch |
| `ui/src/lib/locales/en.json` | `dialogs.listen_together.server_kind*`, `bitchord_server_placeholder` |

## 5. Glass theming

- **`BitChord` theme preset** (`theme.svelte.ts` + `layout.css`): BitChord's light/dark palettes
  (key `#d60017` / `#fa586a`), SF Pro Display for UI *and* lyrics, rounder radius, deeper dark
  shadows. Picked from the normal Themes tab like any other preset.
- **Glass recipes**: `.bc-glass-pill` (fill + `saturate(2.2) blur(16px)` + drop shadow on `::before`,
  four 0.5px inset-stroke shadows on `::after`) and `.bc-glass-material` (fill + blur), driven by
  new `--glass-*` tokens with light/dark values and a `prefers-contrast` fallback. Used by the
  BitChord player pill; available to anything that wants the same material.
- **SF Pro Display** (regular/bold WOFF2, from the BitChord player) bundled at
  `ui/src/lib/fonts/` with BitChord's unicode-range `@font-face`; wired through `--font-lyrics`
  and the BitChord theme. Under its own family name, so a locally installed SF Pro can't stand in.

## 6. Settings reference (new keys)

| Key | Where | Values | Default |
| --- | --- | --- | --- |
| `player_style` | Playback ▸ Interface | `limusic` / `bitchord` | `limusic` |
| `island_enabled` | General ▸ System | `true` / `false` | on |
| `lyricsAnim` (appearance, localStorage) | Appearance ▸ Player view | + `wipe` | unchanged |
| `lt_server_kind` | Listen Together dialog ▸ server panel | `limusic` / `bitchord` | `limusic` |
| appearance `theme` (localStorage) | Themes tab | + `bitchord` | unchanged |

## 7. Building

Unchanged from limusic: Node 22+ / pnpm / Rust stable, `pnpm install && pnpm build` in `ui/`, then
`cargo tauri build` (or your platform's script). The UI was verified with `vite build`,
`svelte-check` (0 errors / 0 warnings) and limusic's 26 self-checks; the Rust additions were
reviewed by hand (no toolchain here to run `cargo check`) — they follow the patterns of the files
they were ported from, and every call site they touch was verified against the existing
signatures.

## 8. Known deviations from the originals

- The lyrics wipe is a reimplementation, not the `am-lyrics` web component: same language
  (wipe/rise/unblur/dim), same timings sourced from limusic's providers; things the component did
  that limusic's data can't express (syllable-level runs, background-vocal sub-lines, duet
  alignment) have no input here and are skipped.
- The island's placeholder mark is a neutral music note (BitChord's logo stayed behind), and the
  accent follows limusic's theme (`var(--primary)` with the BitChord pink as fallback).
- BitChord's island synced at ~900 ms throttle from the page's mediaSession; here it rides
  limusic's 4 Hz ticks — smoother, and the island's own 10 Hz interpolation is unchanged.
- BitChord party identity is per-process (see §4); two BitChord members from the same limusic
  install across restarts are distinct seats until the server times the old one out.
- `LT ▸ transfer host` and per-track volume sync are no-ops in BitChord rooms (the protocol has
  neither).
