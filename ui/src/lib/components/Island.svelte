<script lang="ts">
        /**
         * The Dynamic Island's brain — BitChord's island (itself from Airbeats-Desktop) re-hosted on
         * limusic's architecture.
         *
         * State flows in one way: the backend broadcasts every playback change to every window
         * (`position` at ~4 Hz, `now-playing`, `playback-state`, `playback-params`), the layout runs
         * `initApp(true)` here like it does for the mini widget, and this component reads the shared
         * `playback` store. The original eval'd a snapshot into the page on every media update because
         * BitChord's webview owned playback; limusic's backend owns it, so the plumbing is the store and
         * the window label. The one shell push that remains is focus: Rust evals
         * `window.__IslandFocus(<bool>)` whenever the main window's focus changes, with one boot-time ask
         * (`island_focus_state`) that covers an island born into an already-unfocused app.
         *
         * Presses flow out through the same commands the original used: `island_action` for transport
         * (Rust routes it through the same AppState calls the tray menu items make), `island_resize` to
         * grow/shrink the OS window around the animated card, `island_overlay` to hand the hidden
         * rectangle back to the desktop.
         *
         * Behaviour (unchanged): the island is limusic's glance, not its second player. It exists only
         * while the main window is *not* focused (with focus it would repeat what the app's own screen
         * already says), folds away when the player has nothing to show — a pause is not nothing: the
         * pill holds its ground, play button and all — and a new track never expands it. The card grows
         * only under the user's own click, then collapses after a few seconds. A right-click dismisses
         * it for this stretch of playback; it comes back only after the user has returned to the app and
         * left it again, never on a song change. Long titles and artists crawl instead of truncating
         * (the tape below), and the drawing follows the app's own scrubber: a `--progress` transform on
         * the fill, seek on release, steady playback drawn ten times a second, drags at the frame rate.
         */
        import { onMount } from 'svelte';
        import '@fontsource-variable/dm-sans';
        import './island.css';
        import { invoke } from '@tauri-apps/api/core';
        import { playback } from '$lib/player.svelte';
        import { thumb } from '$lib/thumb';

        /** Every press is best-effort: the overlay is cosmetic; nothing here may take the app down. */
        function press(cmd: string, args?: Record<string, unknown>): void {
                try {
                        void invoke(cmd, args ?? {}).catch(() => {});
                } catch {
                        /* a dead IPC must not take the island down either */
                }
        }

        /** The same, for the asks whose landing the page animates against: the shell resolves the
         *  command once it has been handed to the main thread. */
        function invokeAsync(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
                try {
                        return invoke(cmd, args ?? {}).catch(() => undefined);
                } catch {
                        return Promise.resolve();
                }
        }

        /** "3:21" → 201, tolerating the mpv tick that hasn't landed yet for a brand-new track. */
        function parseSecs(d?: string): number {
                if (!d) return 0;
                const parts = d.split(':').map(Number);
                if (!parts.length || parts.some(Number.isNaN)) return 0;
                return parts.reduce((a, b) => a * 60 + b, 0);
        }

        // ---- Reactive metadata (template + effects) ---------------------------------

        const title = $derived(playback.now?.title ?? '');
        const artist = $derived(playback.now?.artists ?? '');
        const artworkUrl = $derived(playback.now?.thumbnail ?? null);
        const artSrc = $derived(artworkUrl ? (thumb(artworkUrl, 48) ?? '') : '');
        const paused = $derived(playback.paused);
        const hasTrack = $derived(!!playback.now);

        function duration(): number {
                if (playback.duration > 0) return playback.duration;
                return parseSecs(playback.now?.duration);
        }

        // ---- The tape ---------------------------------------------------------------

        /** Long names crawl instead of truncating. Each text line is a clipping box (.mini-title,
         *  .title, .artist) over an inline .tape that carries the words; when the words don't fit, the
         *  box gets the `scrolling` class, the distance on --tape-shift and the pace on
         *  --tape-duration, and the CSS ping-pongs the tape between its ends. The box clips, so nothing
         *  is ever cut — and a name that fits never crawls. */
        const TAPE_SPEED = 24; // scrolled pixels per second: the "slowly" cadence
        /** Sub-this-much overflow is kerning dust, not a second line worth of words. */
        const TAPE_MIN_SHIFT = 4;

        /** A face's boxes are only measurable while the card is laid out at that face's own width — the
         *  other face is absolutely positioned over it and borrows whatever width the card currently
         *  has. Measuring the expanded card's lines while the pill is up would read the pill's 216px as
         *  their box, and the crawl would start against a lie. */
        const TAPE_FACE_WIDTH = { mini: 216, full: 432 } as const;

        interface Tape {
                box: HTMLElement;
                tape: HTMLElement;
                /** Which face of the card the box lives on ('mini' or 'full'). */
                face: keyof typeof TAPE_FACE_WIDTH;
                /** The shift currently applied, so a re-measure can tell change from churn. */
                applied: number;
        }

        let islandEl = $state<HTMLElement>();
        let miniTitleBox = $state<HTMLElement>();
        let fullTitleBox = $state<HTMLElement>();
        let fullArtistBox = $state<HTMLElement>();

        function tapeOf(box: HTMLElement | undefined, face: Tape['face']): Tape | null {
                const tape = box?.querySelector<HTMLElement>('.tape');
                if (!box || !tape) return null;
                return { box, tape, face, applied: 0 };
        }

        let tapes: Tape[] = $state([]);

        function measureTape(entry: Tape): void {
                if (!islandEl) return;
                if (Math.abs(islandEl.clientWidth - TAPE_FACE_WIDTH[entry.face]) > 0.5) return;
                const shift = Math.round(entry.tape.getBoundingClientRect().width - entry.box.clientWidth);
                const scrolling = shift >= TAPE_MIN_SHIFT;
                if (!scrolling) {
                        entry.applied = 0;
                        entry.box.classList.remove('scrolling');
                        return;
                }
                // One-way is 36% of the cycle (the keyframes hold the rest), so the pace on screen stays
                // TAPE_SPEED regardless of how far the words reach.
                const durationShift = shift / (TAPE_SPEED * 0.36);
                entry.box.style.setProperty('--tape-shift', `${-shift}px`);
                entry.box.style.setProperty('--tape-duration', `${durationShift.toFixed(2)}s`);
                if (entry.box.classList.contains('scrolling') && Math.abs(shift - entry.applied) <= 1) return;
                entry.applied = shift;
                entry.box.classList.remove('scrolling');
                void entry.box.offsetWidth; // flush, so a re-armed crawl starts from its rest
                entry.box.classList.add('scrolling');
        }

        function measureTapes(): void {
                for (const entry of tapes) measureTape(entry);
        }

        // ---- Visible state ----------------------------------------------------------

        const COLLAPSE_AFTER_MS = 5000; // Airbeats' dwell time for an expanded card
        const RESIZE_SETTLE_MS = 440; // the card's own width/height transition

        /** The visible state, also mirrored into the OS window's size. */
        let mode: 'hidden' | 'mini' | 'full' = $state('hidden');
        let collapseTimer: ReturnType<typeof setTimeout> | null = null;
        let pendingShrink: ReturnType<typeof setTimeout> | null = null;
        /** Fraction while dragging the seek line, else null. */
        let dragging: number | null = null;
        let rafId = 0;
        let lastDraw = -Infinity;
        /** Stamps each expand request; a collapsed (or re-hidden) card leaves the late-arriving
         *  window-grow with nothing to animate. */
        let expandToken = 0;
        /** Whether the app owns the focus. The shell evals every change; the one boot-time query covers
         *  an island that loads while the app is already in the background. Optimistically focused:
         *  until the answer lands, the card stays away — a wrong guess would flash the island over the
         *  app's own screen. */
        let appFocused = true;
        /** A right-click dismissal lasts until the user has been back in the app (which clears it, see
         *  __IslandFocus) and left again. Song changes, play and pause must not undo it. */
        let dismissed = false;
        /** The scrubbed spot is held until the shell's next position tick confirms it, so the line does
         *  not snap backwards mid-flight. */
        let holdPos = 0;
        let holdUntil = -1;

        function fmt(seconds: number): string {
                if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
                const total = Math.floor(seconds);
                const m = Math.floor(total / 60);
                const s = total % 60;
                return `${m}:${s.toString().padStart(2, '0')}`;
        }

        /** Position carried forward by the rate — the backend ticks at ~4 Hz; this is what makes the
         *  line move like the app's does. */
        function livePosition(now: number): number {
                if (dragging !== null) return dragging * (duration() || 0);
                if (now < holdUntil) return holdPos;
                const base = playback.position;
                if (playback.paused) return base;
                return base + ((now - playback.positionAt) / 1000) * (playback.speed || 1);
        }

        let fillEl = $state<HTMLElement>();
        let elapsedEl = $state<HTMLElement>();
        let remainingEl = $state<HTMLElement>();
        let miniRemainingEl = $state<HTMLElement>();

        function draw(now: number): void {
                if (!fillEl) return;
                const dur = duration();
                const live = livePosition(now);
                const position = Math.min(Math.max(live, 0), dur || Math.max(0, live));
                const progress = dur > 0 ? Math.min(1, Math.max(0, position / dur)) : 0;
                fillEl.style.setProperty('--progress', progress.toFixed(4));
                if (elapsedEl) elapsedEl.textContent = fmt(position);
                const remaining = dur > 0 ? fmt(Math.max(0, dur - position)) : '0:00';
                if (remainingEl) remainingEl.textContent = `-${remaining}`;
                if (miniRemainingEl) miniRemainingEl.textContent = dur > 0 ? `-${remaining}` : '';
        }

        /** The scrubber's idle cadence: ten draws a second, frame-rate while a drag holds the line,
         *  nothing at all when hidden (a hidden webview paints nothing, but neither should its loop
         *  burn cycles). The mini pill draws too: its remaining-time label rides the same clock. */
        function tick(now: number): void {
                rafId = 0;
                if (mode === 'hidden') return;
                const due = dragging !== null || now - lastDraw >= 100;
                if (due) {
                        lastDraw = now;
                        draw(now);
                }
                rafId = requestAnimationFrame(tick);
        }

        function schedule(): void {
                if (!rafId && mode !== 'hidden') rafId = requestAnimationFrame(tick);
        }

        function setClasses(next: typeof mode): void {
                const el = islandEl;
                if (!el) return;
                el.classList.toggle('hidden', next === 'hidden');
                el.classList.toggle('mini', next === 'mini');
                el.classList.toggle('full', next === 'full');
        }

        /** The spawn: the card pops in from nothing — a quick grow with a settle at the end, all inside
         *  the pill's own box so the OS window (which hugs the card) never clips it. Applied whenever
         *  the card comes out of 'hidden'; the class removes itself on animationend, and a mode change
         *  first tears any running spawn down so a fresh one starts from its own first frame. */
        function playSpawn(): void {
                const el = islandEl;
                if (!el) return;
                el.classList.remove('spawn');
                void el.offsetWidth; // flush, so removing and adding restarts the animation
                el.classList.add('spawn');
        }

        /** Two painted frames. The shell resolves `island_resize` when the main thread has the geometry
         *  task; by the compositor's next paint that task has run, so the morph never starts inside a
         *  window still clipping it. (In a plain browser there is no shell and no clipping; the promise
         *  resolves at once and the frames are all the wait there is.) */
        function nextPaint(): Promise<void> {
                return new Promise((resolve) => {
                        requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
                });
        }

        function applyMode(next: typeof mode): void {
                if (mode === next) return;
                const previous = mode;
                mode = next;
                // The window hugs the card. Expanding grows the window first and only then starts the morph
                // — a 216px window would clip the growing card's first frames and read as a stutter.
                // Collapsing animates at once (the big window has room), then shrinks once the card has
                // settled — whatever it collapsed into: a pause while expanded must hand the small window
                // back too, or the resumed pill sits in a 432px window that eats clicks around it.
                if (next === 'full') {
                        if (pendingShrink !== null) {
                                clearTimeout(pendingShrink);
                                pendingShrink = null;
                        }
                        const token = ++expandToken;
                        const spawning = previous === 'hidden';
                        void invokeAsync('island_resize', { expanded: true })
                                .then(() => nextPaint())
                                .then(() => {
                                        if (expandToken !== token || mode !== 'full') return;
                                        setClasses('full');
                                        if (spawning) playSpawn();
                                        // The morph's width transition can drop its last ResizeObserver callback (browsers
                                        // coalesce late frames), which would leave the tape's crawl distance measured
                                        // against a mid-flight width. Once the transition has settled, measure against
                                        // the settled box. The crawl starts 0.8s after re-arming, so this lands before
                                        // it moves.
                                        setTimeout(measureTapes, RESIZE_SETTLE_MS + 80);
                                });
                } else {
                        ++expandToken;
                        setClasses(next);
                        // Out of nothing: the pill pops in (see playSpawn). Between the two visible states the
                        // card simply morphs — no spawn there.
                        if (previous === 'hidden') playSpawn();
                        if (previous === 'full') {
                                if (pendingShrink !== null) clearTimeout(pendingShrink);
                                pendingShrink = setTimeout(() => {
                                        pendingShrink = null;
                                        press('island_resize', { expanded: false });
                                }, RESIZE_SETTLE_MS);
                        }
                        // Same settle for the collapse: the mini face gets its real width back.
                        setTimeout(measureTapes, RESIZE_SETTLE_MS + 80);
                }
                press('island_overlay', { hidden: next === 'hidden' });
                lastDraw = -Infinity;
                schedule();
        }

        function cancelCollapse(): void {
                if (collapseTimer !== null) {
                        clearTimeout(collapseTimer);
                        collapseTimer = null;
                }
        }

        function scheduleCollapse(): void {
                cancelCollapse();
                collapseTimer = setTimeout(() => {
                        collapseTimer = null;
                        if (mode === 'full') applyMode('mini');
                }, COLLAPSE_AFTER_MS);
        }

        function expand(): void {
                applyMode('full');
                scheduleCollapse();
        }

        function collapse(): void {
                cancelCollapse();
                if (mode === 'full') applyMode('mini');
        }

        /** The one policy for what the island shows, applied on every input — a playback change, a focus
         *  change, the boot-time focus answer. Nothing to show wins first (a track-less player is
         *  nothing — no title and no artwork), then the app's own focus (the page already shows
         *  everything the card would say), then a dismissal. A pause is *not* nothing: the pill holds
         *  its ground with its play button, like the phone's does — only the user's own right-click
         *  sends it away mid-track. Only when there is a track, the app is in the background and the
         *  user hasn't dismissed the island does the card exist; coming out of 'hidden' it spawns.
         *  Song changes are *not* an input: they refresh what the card shows, never whether it shows. */
        function reconcile(): void {
                if (!playback.now) {
                        cancelCollapse();
                        applyMode('hidden');
                        return;
                }
                if (appFocused || dismissed) {
                        cancelCollapse();
                        applyMode('hidden');
                        return;
                }
                if (mode === 'hidden') applyMode('mini');
        }

        // ---- Store → card wiring ----------------------------------------------------

        // Metadata: words and artwork land in the DOM through the bindings above; this effect only
        // measures the tapes (a crawl re-arms against the real box) and re-decides what to show.
        $effect(() => {
                void title;
                void artist;
                void artSrc;
                void hasTrack;
                measureTapes();
                reconcile();
        });

        // Focus pushes from the shell (island.rs::notify_main_focus), and the same place a dismissal is
        // cleared: being back in the app re-arms the island, so it returns only when the user leaves the
        // app again — never on a song switch.
        let islandFocusHandler: ((focused: boolean) => void) | null = null;

        onMount(() => {
                // Tapes bind after mount; rebuild the list against the real boxes. The metadata effect ran
                // before this with an empty list, so measure once here — a track already playing must crawl
                // from its first paint, not from its next metadata change.
                tapes = [
                        tapeOf(miniTitleBox, 'mini'),
                        tapeOf(fullTitleBox, 'full'),
                        tapeOf(fullArtistBox, 'full')
                ].filter((t): t is Tape => t !== null);
                measureTapes();

                // The card's width is transitioned, so a morph re-fires the observers frame by frame until
                // the destination width lands and the guard above lets the measurement through — the crawl
                // always settles against the real box.
                const tapeObserver = new ResizeObserver(measureTapes);
                for (const entry of tapes) tapeObserver.observe(entry.box);
                // Fonts arrive after the first paint, and a swap changes the *words'* width, not the box's —
                // the observers never see it. `fonts.ready` is not enough: it can resolve before the
                // @font-face downloads have even started (the loading they trigger begins at first render),
                // so the settle is also taken from the FontFaceSet's own `loadingdone` — the event that
                // fires when the real faces finish — with the ladder as backstop for late swaps.
                if (document.fonts?.ready) {
                        void document.fonts.ready.then(() => {
                                measureTapes();
                                requestAnimationFrame(measureTapes);
                                setTimeout(measureTapes, 300);
                                setTimeout(measureTapes, 900);
                        });
                }
                document.fonts?.addEventListener?.('loadingdone', measureTapes);

                islandFocusHandler = (focused: boolean) => {
                        appFocused = focused;
                        if (focused) dismissed = false;
                        reconcile();
                };
                (window as unknown as { __IslandFocus?: typeof islandFocusHandler }).__IslandFocus =
                        islandFocusHandler;

                // The boot-time ask, so an island created while the app is already in the background (the
                // settings toggle, or a session restored to the tray) knows it may show before the next
                // Focused event crosses the shell.
                void invokeAsync('island_focus_state').then((focused) => {
                        appFocused = focused !== false;
                        reconcile();
                });

                // ---- The presses ----------------------------------------------------------

                const island = islandEl!;
                // The card expands on a click of the pill itself; the controls are excluded, like Airbeats
                // excluded its own.
                const onClick = (event: MouseEvent) => {
                        if (mode !== 'mini') return;
                        if ((event.target as HTMLElement | null)?.closest('button')) return;
                        expand();
                };
                // A right-click sends the island away for this stretch of listening — not a collapse to
                // mini, a dismissal. It returns only through reconcile's rules: the user has to have been
                // back in the app (which clears `dismissed` in __IslandFocus) and left the app again.
                // Nothing else — not a song change, not play or pause — revives it.
                const onContext = (event: MouseEvent) => {
                        event.preventDefault();
                        dismissed = true;
                        cancelCollapse();
                        applyMode('hidden');
                };
                island.addEventListener('click', onClick);
                island.addEventListener('contextmenu', onContext);
                // Focus leaving the island folds it back, the way the phone's card folds when you touch
                // elsewhere. The auto-collapse timer covers the no-click case.
                const onBlur = () => {
                        if (dragging !== null) return;
                        collapse();
                };
                const onAnimationEnd = () => island.classList.remove('spawn');
                island.addEventListener('animationend', onAnimationEnd);
                window.addEventListener('blur', onBlur);

                // The seek line. Pressing or dragging seeks on release; the picked spot is held until the
                // shell's next position sync confirms it.
                const scrubEl = island.querySelector<HTMLElement>('#island-scrub');
                const railEl = fillEl?.parentElement ?? null;
                const fractionAt = (event: PointerEvent): number => {
                        const rect = railEl?.getBoundingClientRect();
                        if (!rect || rect.width <= 0) return 0;
                        return Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width));
                };
                const onPointerDown = (event: PointerEvent) => {
                        if (duration() <= 0) return;
                        dragging = fractionAt(event);
                        scrubEl?.classList.add('dragging');
                        scrubEl?.setPointerCapture(event.pointerId);
                        lastDraw = -Infinity;
                        schedule();
                };
                const onPointerMove = (event: PointerEvent) => {
                        if (dragging === null) return;
                        dragging = fractionAt(event);
                        lastDraw = -Infinity;
                        schedule();
                };
                const endDrag = (event: PointerEvent) => {
                        if (dragging === null) return;
                        const fraction = fractionAt(event);
                        dragging = null;
                        scrubEl?.classList.remove('dragging');
                        const dur = duration();
                        if (dur > 0) {
                                const target = fraction * dur;
                                press('island_action', { action: 'seekto', arg: target });
                                // Hold the picked spot until the next position tick lands (the optimistic write is
                                // what the app's own scrubber does with `playback.position`).
                                holdPos = target;
                                holdUntil = performance.now() + 700;
                                playback.position = target;
                                playback.positionAt = performance.now();
                        }
                        scheduleCollapse();
                };
                scrubEl?.addEventListener('pointerdown', onPointerDown);
                scrubEl?.addEventListener('pointermove', onPointerMove);
                scrubEl?.addEventListener('pointerup', endDrag);
                scrubEl?.addEventListener('pointercancel', endDrag);

                // Artwork arrives or fails independent of the state syncs.
                const imgListeners: Array<() => void> = [];
                for (const img of island.querySelectorAll<HTMLImageElement>('.art img')) {
                        const onLoad = () => img.classList.add('loaded');
                        const onError = () => {
                                img.classList.remove('loaded');
                                img.parentElement?.classList.add('bare');
                        };
                        img.addEventListener('load', onLoad);
                        img.addEventListener('error', onError);
                        imgListeners.push(() => {
                                img.removeEventListener('load', onLoad);
                                img.removeEventListener('error', onError);
                        });
                }

                schedule();

                return () => {
                        cancelCollapse();
                        if (pendingShrink !== null) clearTimeout(pendingShrink);
                        if (rafId) cancelAnimationFrame(rafId);
                        rafId = 0;
                        tapeObserver.disconnect();
                        document.fonts?.removeEventListener?.('loadingdone', measureTapes);
                        island.removeEventListener('click', onClick);
                        island.removeEventListener('contextmenu', onContext);
                        island.removeEventListener('animationend', onAnimationEnd);
                        window.removeEventListener('blur', onBlur);
                        for (const off of imgListeners) off();
                        for (const el of [miniTitleBox, fullTitleBox, fullArtistBox]) {
                                el?.classList.remove('scrolling');
                        }
                        delete (window as unknown as { __IslandFocus?: unknown }).__IslandFocus;
                };
        });

        function onPrev() {
                press('island_action', { action: 'previoustrack' });
                scheduleCollapse();
        }
        function onNext() {
                press('island_action', { action: 'nexttrack' });
                scheduleCollapse();
        }
        function onPlayPause() {
                // Explicit play/pause, resolved Rust-side against the same flag MPRIS reads, so the island's
                // button and the app's can't disagree.
                press('island_action', { action: paused ? 'play' : 'pause' });
                scheduleCollapse();
        }
        function onOpen() {
                // The artwork raises the app, as the LCD's opens the player view.
                press('island_action', { action: 'open' });
        }
</script>

<!-- The two states of the card. The OS window around this page grows and shrinks with it
     (island.rs::apply_size) — a transparent window still eats clicks over its invisible areas, so
     it hugs the pill at all times. The state classes here are imperative, never reactive: the
     morph must not start until the OS window has grown around it (setClasses runs after the
     resize promise and two painted frames), and Svelte re-owning this attribute would wipe the
     spawn/morph timing the choreography depends on. -->
<div bind:this={islandEl} class="island hidden">
        <!-- Mini: artwork, name, remaining — the pill that survives a glance. -->
        <div class="mini">
                <span class="art" class:bare={!artworkUrl}>
                        <img src={artSrc} alt="" draggable="false" />
                        <svg class="logo" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
                                <path
                                        fill="currentColor"
                                        d="M9 18.5a3 3 0 1 1-2-2.83V6.2a1 1 0 0 1 .76-.97l8-2A1 1 0 0 1 17 4.2v2.05a1 1 0 0 1-.76.97L9 8.98z"
                                />
                        </svg>
                </span>
                <!-- The .tape inside each text line is what crawls when the words don't fit (island.css's
                     tape-crawl); the line itself only clips. -->
                <span class="mini-title" bind:this={miniTitleBox}><span class="tape">{title}</span></span>
                <span class="mini-remaining" bind:this={miniRemainingEl}></span>
        </div>

        <!-- Full: the LCD line, the seek line, the transport. -->
        <div class="full">
                <div class="now">
                        <button class="art" class:bare={!artworkUrl} onclick={onOpen} aria-label="Open Limusic" title="Open Limusic">
                                <img src={artSrc} alt="" draggable="false" />
                                <svg class="logo" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
                                        <path
                                                fill="currentColor"
                                                d="M9 18.5a3 3 0 1 1-2-2.83V6.2a1 1 0 0 1 .76-.97l8-2A1 1 0 0 1 17 4.2v2.05a1 1 0 0 1-.76.97L9 8.98z"
                                        />
                                </svg>
                        </button>
                        <span class="meta">
                                <span class="title" bind:this={fullTitleBox}><span class="tape">{title}</span></span>
                                <span class="artist" bind:this={fullArtistBox}><span class="tape">{artist}</span></span>
                        </span>
                </div>

                <div class="scrub" id="island-scrub" data-scrubber>
                        <span class="time elapsed" bind:this={elapsedEl}>0:00</span>
                        <div class="rail">
                                <div class="fill" bind:this={fillEl}></div>
                        </div>
                        <span class="time remaining" bind:this={remainingEl}>0:00</span>
                </div>

                <div class="transport">
                        <button class="control" onclick={onPrev} aria-label="Previous">
                                <svg width="26" height="26" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true" style="transform: scaleX(-1)">
                                        <path
                                                d="M18.14 20.68c.365 0 .672-.107 1.038-.323l8.508-4.997c.623-.365.938-.814.938-1.37 0-.564-.307-.988-.938-1.361l-8.508-4.997c-.366-.216-.68-.324-1.046-.324-.73 0-1.337.556-1.337 1.569v4.773c-.108-.399-.406-.73-.904-1.021L7.382 7.632c-.357-.216-.672-.324-1.037-.324-.73 0-1.345.556-1.345 1.569v10.235c0 1.013.614 1.569 1.345 1.569.365 0 .68-.108 1.037-.324l8.509-4.997c.49-.29.796-.631.904-1.038v4.79c0 1.013.615 1.569 1.345 1.569z"
                                        />
                                </svg>
                        </button>
                        <button class="control play" class:paused onclick={onPlayPause} aria-label={paused ? 'Play' : 'Pause'}>
                                <svg class="icon-pause" width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                        <path
                                                d="M13.293 22.772c.955 0 1.436-.481 1.436-1.436V6.677c0-.98-.481-1.427-1.436-1.427h-2.457c-.954 0-1.436.473-1.436 1.427v14.66c-.008.954.473 1.435 1.436 1.435h2.457zm7.87 0c.954 0 1.427-.481 1.427-1.436V6.677c0-.98-.473-1.427-1.428-1.427h-2.465c-.955 0-1.428.473-1.428 1.427v14.66c0 .954.473 1.435 1.428 1.435h2.465z"
                                        />
                                </svg>
                                <svg class="icon-play" width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                        <path
                                                d="M10.345 23.287c.415 0 .763-.15 1.22-.407l12.742-7.404c.838-.481 1.178-.855 1.178-1.46 0-.599-.34-.972-1.178-1.462L11.565 5.158c-.457-.265-.805-.407-1.22-.407-.789 0-1.345.606-1.345 1.57V21.71c0 .971.556 1.577 1.345 1.577z"
                                        />
                                </svg>
                        </button>
                        <button class="control" onclick={onNext} aria-label="Next">
                                <svg width="26" height="26" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                        <path
                                                d="M18.14 20.68c.365 0 .672-.107 1.038-.323l8.508-4.997c.623-.365.938-.814.938-1.37 0-.564-.307-.988-.938-1.361l-8.508-4.997c-.366-.216-.68-.324-1.046-.324-.73 0-1.337.556-1.337 1.569v4.773c-.108-.399-.406-.73-.904-1.021L7.382 7.632c-.357-.216-.672-.324-1.037-.324-.73 0-1.345.556-1.345 1.569v10.235c0 1.013.614 1.569 1.345 1.569.365 0 .68-.108 1.037-.324l8.509-4.997c.49-.29.796-.631.904-1.038v4.79c0 1.013.615 1.569 1.345 1.569z"
                                        />
                                </svg>
                        </button>
                </div>
        </div>
</div>
