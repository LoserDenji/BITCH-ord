<script lang="ts">
        /**
         * The BitChord glass player pill, ported from the BitChord desktop player's `PlayerBar` +
         * `Lcd` + `useScrubber` + `VolumeControl` (React) to limusic's Svelte 5 + store architecture.
         *
         * The original reads a zustand store and an `HTMLAudioElement` the webview owns; this one reads
         * limusic's `playback` store — position arrives at ~4 Hz with `positionAt` stamping when it
         * landed, so the scrubber extrapolates between ticks exactly like the original extrapolated off
         * `audio.currentTime`. The drawing strategy is the original's, verbatim: no reactive state per
         * frame — one rAF-scheduled loop writes `--progress` on the fill and the times into their spans
         * directly (10 Hz while idle, frame-rate while dragging, nothing while the window is hidden),
         * because every reactive write re-renders and re-blurs the glass behind the bar.
         *
         * Three limusic facts shape the adaptation: tempo/pitch exist (displayed time runs at
         * `playback.speed` of track time, while the fill's *fraction* is tempo-invariant), volume is
         * 0–100 (the original's 0–1), and transport goes through `api` commands instead of the store.
         */
        import { onMount } from 'svelte';
        import { goto } from '$app/navigation';
        import * as api from '$lib/api';
        import {
                commitVolume,
                cycleRepeat,
                dragVolume,
                np,
                playback,
                toggleMute,
                toggleNowPlayingRating,
                wheelVolume
        } from '$lib/player.svelte';
        import { thumb } from '$lib/thumb';
        import { appearance } from '$lib/theme.svelte';
        import { t } from '$lib/i18n.svelte';

        let {
                onToggleQueue,
                queueOpen,
                onToggleLyrics,
                lyricsOpen,
                onToggleComments,
                commentsOpen
        }: {
                onToggleQueue: () => void;
                queueOpen: boolean;
                onToggleLyrics: () => void;
                lyricsOpen: boolean;
                onToggleComments: () => void;
                commentsOpen: boolean;
        } = $props();

        // ---- Derived state (the store selectors the original read) ------------------

        const now = $derived(playback.now);
        const hasTrack = $derived(!!now);
        const playing = $derived(!playback.paused);
        const shuffleOn = $derived(playback.queue.shuffle ?? false);
        const repeat = $derived(playback.queue.repeat ?? 'off');
        const rating = $derived(playback.rating);
        // The queue's row for what's playing: it carries the album the title opens (there is no per-song
        // page). Matched on videoId so a mid-gapless-advance mismatch can't point the link at the wrong
        // song, and kept away from local files, which have no album.
        const currentSong = $derived.by(() => {
                const cur = playback.queue.items[playback.queue.currentIndex];
                return cur?.video_id === now?.videoId ? cur : null;
        });
        const albumId = $derived(
                currentSong && !api.isLocalId(currentSong.video_id) ? currentSong.album_id : undefined
        );
        const artSrc = $derived(now?.thumbnail ? (thumb(now.thumbnail, 96) ?? '') : '');
        const volumeLevel = $derived(playback.volume / 100);

        // ---- The scrubber (useScrubber, ported) -------------------------------------

        function fmt(secs: number): string {
                if (!Number.isFinite(secs) || secs < 0) return '0:00';
                const t = Math.floor(secs);
                const h = Math.floor(t / 3600);
                const m = Math.floor((t % 3600) / 60);
                const s = t % 60;
                const mm = h ? m.toString().padStart(2, '0') : `${m}`;
                return `${h ? `${h}:` : ''}${mm}:${s.toString().padStart(2, '0')}`;
        }

        /** Track seconds at the user's tempo, from real playback seconds. */
        function shownSecs(real: number): number {
                return real / (playback.speed || 1);
        }

        /** Where the song is right now, carried forward from the last mpv tick. */
        function liveRealSecs(): number {
                if (playback.paused) return playback.position;
                return playback.position + (performance.now() - playback.positionAt) / 1000;
        }

        const IDLE_FRAME_MS = 100;
        let scrubberEl = $state<HTMLElement>();
        let barEl = $state<HTMLElement>();
        let elapsedEl = $state<HTMLElement>();
        let remainingEl = $state<HTMLElement>();
        /** Fraction of the track while dragging, else null. */
        let dragging = $state<number | null>(null);

        onMount(() => {
                let frame = 0;
                // One pending frame at a time: a direct draw while one is scheduled would start a second
                // loop, and every seek would add another.
                let scheduled = false;
                const kick = () => {
                        if (scheduled) return;
                        scheduled = true;
                        frame = requestAnimationFrame(step);
                };
                let lastDraw = -Infinity;
                const step = () => {
                        scheduled = false;
                        if (document.hidden) return;
                        const nowTs = performance.now();
                        const dur = playback.duration / (playback.speed || 1);
                        const real = dragging !== null && dur > 0 ? dragging * playback.duration : liveRealSecs();
                        const at = shownSecs(real);
                        const base = dur > 0 ? Math.min(1, Math.max(0, at / dur)) : 0;
                        if (dragging !== null || nowTs - lastDraw >= IDLE_FRAME_MS) {
                                lastDraw = nowTs;
                                barEl?.style.setProperty('--progress', String(base));
                                // Identical text is skipped: the engine would otherwise invalidate the span each frame.
                                const atText = fmt(at);
                                if (elapsedEl && elapsedEl.textContent !== atText) elapsedEl.textContent = atText;
                                const left = `-${fmt(Math.max(0, dur - at))}`;
                                if (remainingEl && remainingEl.textContent !== left) remainingEl.textContent = left;
                        }
                        if (!playback.paused || dragging !== null) kick();
                };
                kick();
                const onVisibility = () => {
                        if (document.visibilityState === 'visible') kick();
                };
                document.addEventListener('visibilitychange', onVisibility);
                return () => {
                        cancelAnimationFrame(frame);
                        document.removeEventListener('visibilitychange', onVisibility);
                };
        });

        function fractionAt(event: PointerEvent): number {
                const rect = scrubberEl?.getBoundingClientRect();
                if (!rect || rect.width <= 0) return 0;
                return Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width));
        }

        function onScrubDown(event: PointerEvent) {
                if (!scrubberEl) return;
                scrubberEl.setPointerCapture(event.pointerId);
                dragging = fractionAt(event);
        }

        function onScrubMove(event: PointerEvent) {
                if (dragging === null) return;
                dragging = fractionAt(event);
        }

        function onScrubUp(event: PointerEvent) {
                if (dragging === null) return;
                const fraction = fractionAt(event);
                const dur = playback.duration;
                if (dur > 0) {
                        const v = fraction * dur;
                        playback.position = v; // optimistic — the mpv tick confirms
                        playback.positionAt = performance.now();
                        api.seek(v);
                }
                dragging = null;
        }

        function onScrubCancel() {
                dragging = null;
        }

        // ---- Transport --------------------------------------------------------------

        function onTogglePlay() {
                api.togglePause().catch(() => {});
        }
        function onNext() {
                api.nextTrack().catch(() => {});
        }
        function onPrev() {
                api.prevTrack().catch(() => {});
        }
        function onShuffle() {
                api.toggleShuffle().catch(() => {});
        }

        function toggleLike() {
                toggleNowPlayingRating();
        }

        // ---- Volume (VolumeControl, ported) -----------------------------------------

        // The level slides in over the actions beside it; they fade out while it's open. Opening is the
        // hover itself, a wheel step (each step calls open, so the level shows while it moves), or
        // focusing the speaker.
        let volumeOpen = $state(false);
        let volumeWrap = $state<HTMLElement>();

        function openVolume() {
                volumeOpen = true;
        }

        function onWheel(event: WheelEvent) {
                // The wrapped handler reads the store itself; limusic's wheel volume clamps 0–100.
                wheelVolume(event);
                openVolume();
        }

        onMount(() => {
                const el = volumeWrap;
                if (!el) return;
                // Svelte only forces passive listeners on touch events, not wheel, so the native addEventListener
                // form is only insurance; what it buys is registering before any wheel arrives.
                el.addEventListener('wheel', onWheel, { passive: false });
                return () => el.removeEventListener('wheel', onWheel);
        });

        function onVolumeInput(event: Event) {
                dragVolume(Number((event.target as HTMLInputElement).value));
        }
        function onVolumeCommit(event: Event) {
                commitVolume(Number((event.target as HTMLInputElement).value));
        }

        // The volume echo guard: while a drag is in flight, incoming volume events are skipped by
        // initApp's handler, so the thumb can't be yanked backwards mid-drag.

        // ---- Empty-bar click opens the player view (the LCD's artwork does too) ------

        const isControl = (target: EventTarget | null) =>
                !!(target as HTMLElement | null)?.closest?.('button, a, input, [role="button"]');
        let pressedControl = false;

        function onBarClick(event: MouseEvent) {
                if (!appearance.emptyClickTogglesPlayer || pressedControl || isControl(event.target)) return;
                np.open = !np.open;
        }
</script>

<!-- The floating glass pill. The panels beside it overlay the page; the pill keeps its place. -->
<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions, a11y_no_noninteractive_element_interactions -->
<footer
        data-player-bar
        data-playing={playback.paused ? 'false' : 'true'}
        class="pointer-events-auto flex justify-center px-5 pb-2"
        onpointerdown={(e) => (pressedControl = isControl(e.target))}
        onclick={onBarClick}
>
        <div
                class="bc-glass-pill group/pill relative grid h-14 w-full max-w-170 grid-cols-[184px_minmax(180px,1fr)_minmax(57px,auto)] items-center rounded-full px-4 {commentsOpen ||
                        queueOpen ||
                        lyricsOpen
                        ? 'max-w-170'
                        : ''}"
                role="region"
                aria-label="Player"
        >
                <!-- Transport: shuffle and repeat are secondary and light in the key colour when on, the
                     play/pause pair in the middle. -->
                <div class="flex items-center justify-between pr-2">
                        <button
                                class="grid size-7 cursor-pointer place-items-center rounded-[6px] text-secondary transition-colors hover:text-primary aria-pressed:text-key"
                                aria-label={t('player.shuffle')}
                                aria-pressed={shuffleOn}
                                onclick={onShuffle}
                        >
                                <svg width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                        <path
                                                d="M20.767 20.44a.81.81 0 00.49-.183l2.58-2.174c.316-.266.316-.681 0-.955l-2.58-2.183a.81.81 0 00-.49-.183c-.415 0-.673.258-.673.673v1.245h-1.162c-.739 0-1.195-.233-1.718-.847l-1.527-1.801 1.527-1.81c.54-.63.946-.847 1.677-.847h1.203v1.279c0 .407.258.664.673.664a.801.801 0 00.49-.174l2.58-2.175c.316-.266.316-.69 0-.955l-2.58-2.183a.761.761 0 00-.49-.183c-.415 0-.673.258-.673.665v1.386h-1.212c-1.228 0-1.992.34-2.863 1.386l-1.412 1.668-1.469-1.751c-.805-.946-1.569-1.303-2.747-1.303H8.896c-.53 0-.896.348-.896.838s.365.838.896.838h1.437c.697 0 1.162.225 1.685.847l1.519 1.801-1.52 1.81c-.53.623-.954.847-1.643.847H8.896c-.53 0-.896.348-.896.838s.365.838.896.838h1.536c1.179 0 1.901-.356 2.706-1.303l1.478-1.751 1.444 1.718c.822.98 1.627 1.336 2.822 1.336h1.212v1.412c0 .415.258.672.673.672z"
                                        />
                                </svg>
                        </button>
                        <div class="flex items-center gap-1.5">
                                <button
                                        class="grid size-7 cursor-pointer place-items-center rounded-[6px] text-primary transition-opacity disabled:opacity-40"
                                        disabled={!hasTrack}
                                        aria-label={t('player.previous')}
                                        onclick={onPrev}
                                >
                                        <svg width="26" height="26" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true" style="transform: scaleX(-1)">
                                                <path
                                                        d="M18.14 20.68c.365 0 .672-.107 1.038-.323l8.508-4.997c.623-.365.938-.814.938-1.37 0-.564-.307-.988-.938-1.361l-8.508-4.997c-.366-.216-.68-.324-1.046-.324-.73 0-1.337.556-1.337 1.569v4.773c-.108-.399-.406-.73-.904-1.021L7.382 7.632c-.357-.216-.672-.324-1.037-.324-.73 0-1.345.556-1.345 1.569v10.235c0 1.013.614 1.569 1.345 1.569.365 0 .68-.108 1.037-.324l8.509-4.997c.49-.29.796-.631.904-1.038v4.79c0 1.013.615 1.569 1.345 1.569z"
                                                />
                                        </svg>
                                </button>
                                <button
                                        class="grid size-7 cursor-pointer place-items-center rounded-[6px] text-primary disabled:opacity-40"
                                        disabled={!hasTrack}
                                        aria-label={playing ? t('player.pause') : t('player.play')}
                                        onclick={onTogglePlay}
                                >
                                        {#if playing}
                                                <svg width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                                        <path
                                                                d="M13.293 22.772c.955 0 1.436-.481 1.436-1.436V6.677c0-.98-.481-1.427-1.436-1.427h-2.457c-.954 0-1.436.473-1.436 1.427v14.66c-.008.954.473 1.435 1.436 1.435h2.457zm7.87 0c.954 0 1.427-.481 1.427-1.436V6.677c0-.98-.473-1.427-1.428-1.427h-2.465c-.955 0-1.428.473-1.428 1.427v14.66c0 .954.473 1.435 1.428 1.435h2.465z"
                                                        />
                                                </svg>
                                        {:else}
                                                <svg width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                                        <path
                                                                d="M10.345 23.287c.415 0 .763-.15 1.22-.407l12.742-7.404c.838-.481 1.178-.855 1.178-1.46 0-.599-.34-.972-1.178-1.462L11.565 5.158c-.457-.265-.805-.407-1.22-.407-.789 0-1.345.606-1.345 1.57V21.71c0 .971.556 1.577 1.345 1.577z"
                                                        />
                                                </svg>
                                        {/if}
                                </button>
                                <button
                                        class="grid size-7 cursor-pointer place-items-center rounded-[6px] text-primary disabled:opacity-40"
                                        disabled={!hasTrack}
                                        aria-label={t('player.next')}
                                        onclick={onNext}
                                >
                                        <svg width="26" height="26" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M18.14 20.68c.365 0 .672-.107 1.038-.323l8.508-4.997c.623-.365.938-.814.938-1.37 0-.564-.307-.988-.938-1.361l-8.508-4.997c-.366-.216-.68-.324-1.046-.324-.73 0-1.337.556-1.337 1.569v4.773c-.108-.399-.406-.73-.904-1.021L7.382 7.632c-.357-.216-.672-.324-1.037-.324-.73 0-1.345.556-1.345 1.569v10.235c0 1.013.614 1.569 1.345 1.569.365 0 .68-.108 1.037-.324l8.509-4.997c.49-.29.796-.631.904-1.038v4.79c0 1.013.615 1.569 1.345 1.569z"
                                                />
                                        </svg>
                                </button>
                        </div>
                        <button
                                class="grid size-7 cursor-pointer place-items-center rounded-[6px] text-secondary transition-colors hover:text-primary aria-pressed:text-key"
                                aria-label={t('player.repeat_state', {
                                        state: repeat === 'off' ? t('player.repeat_off') : repeat === 'one' ? t('player.repeat_one') : t('player.repeat_all')
                                })}
                                aria-pressed={repeat !== 'off'}
                                onclick={() => cycleRepeat()}
                        >
                                {#if repeat === 'one'}
                                        <svg width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M22.752 12.313c.473 0 .747-.257.747-.771V8.503c0-.54-.357-.904-.888-.904-.44 0-.698.14-1.038.398l-.838.656c-.2.15-.266.299-.266.473 0 .257.19.465.498.465.133 0 .24-.042.349-.125l.614-.514h.058v2.59c0 .514.274.771.764.771zm-13.207 1.96a.84.84 0 00.863-.856v-.448c0-1.004.706-1.677 1.785-1.677h3.403v1.362c0 .407.258.664.673.664a.745.745 0 00.49-.183l2.581-2.166c.316-.266.316-.69 0-.955L16.76 7.831a.745.745 0 00-.49-.183c-.415 0-.673.258-.673.665v1.294h-3.278c-2.217 0-3.628 1.254-3.628 3.213v.597c0 .49.374.855.855.855zm4.864 5.951c.407 0 .664-.257.664-.664v-1.303h4.881c2.225 0 3.628-1.254 3.628-3.213v-.597a.838.838 0 00-.855-.855.833.833 0 00-.855.855v.448c0 1.004-.714 1.677-1.793 1.677h-5.006v-1.353c0-.407-.257-.664-.664-.664a.767.767 0 00-.498.182l-2.573 2.175c-.324.257-.315.68 0 .946l2.573 2.192a.807.807 0 00.498.174z"
                                                />
                                        </svg>
                                {:else}
                                        <svg width="28" height="28" viewBox="2 0 28 28" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M9.545 14.272a.856.856 0 00.863-.855v-.448c0-1.004.706-1.677 1.785-1.677h5.005v1.362c0 .407.258.664.673.664a.745.745 0 00.49-.183l2.581-2.166c.316-.266.316-.69 0-.955l-2.581-2.183a.745.745 0 00-.49-.183c-.415 0-.672.258-.672.665v1.294h-4.881c-2.217 0-3.628 1.254-3.628 3.213v.597c0 .474.382.855.855.855zm4.864 5.952c.407 0 .664-.257.664-.664v-1.303h4.881c2.225 0 3.628-1.254 3.628-3.213v-.597a.854.854 0 10-1.71 0v.448c0 1.004-.714 1.677-1.793 1.677h-5.006v-1.353c0-.407-.257-.664-.664-.664a.767.767 0 00-.498.182l-2.573 2.175c-.324.257-.315.68 0 .946l2.573 2.192a.807.807 0 00.498.174z"
                                                />
                                        </svg>
                                {/if}
                        </button>
                </div>

                <!-- The LCD: artwork, names, and the seek line that lives on the tile's bottom edge. -->
                <div class="h-full min-w-0 px-4">
                        <div class="group/lcd relative grid h-full grid-cols-[34px_minmax(0,1fr)] items-center gap-x-2 pb-1.5">
                                <!-- As Apple's: the artwork opens the now-playing view, with an expand glyph on hover. -->
                                <button
                                        class="relative block h-[34px] w-[34px] cursor-pointer overflow-hidden rounded-[6px] bg-joe p-0 transition-opacity"
                                        aria-label={t('player.open_player')}
                                        onclick={() => (np.open = true)}
                                >
                                        {#if artSrc}
                                                <img src={artSrc} alt="" class="size-full object-cover" loading="lazy" />
                                        {/if}
                                        <!-- Hovering anywhere on the pill dims the artwork and shows the expand glyph. -->
                                        <span
                                                class="absolute inset-0 grid place-items-center bg-[rgb(0_0_0/0.5)] text-white opacity-0 transition-opacity duration-200 group-hover/pill:opacity-100"
                                                aria-hidden="true"
                                        >
                                                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                                        <polyline points="15 3 21 3 21 9" />
                                                        <polyline points="9 21 3 21 3 15" />
                                                        <line x1="21" y1="3" x2="14" y2="10" />
                                                        <line x1="3" y1="21" x2="10" y2="14" />
                                                </svg>
                                        </span>
                                </button>
                                <!-- Over the seek line, the names soften into the background so the times can sit over
                                     them (the original's group-has-scrubber blur). -->
                                <div class="flex min-w-0 flex-col transition-[filter,opacity] duration-200 group-has-[[data-scrubber]:hover]/lcd:opacity-30 group-has-[[data-scrubber]:hover]/lcd:blur-[3px] group-has-[[data-dragging]]/lcd:opacity-30 group-has-[[data-dragging]]/lcd:blur-[3px]">
                                        {#if now?.title}
                                                <div class="flex min-w-0 items-center">
                                                        {#if albumId}
                                                                <button
                                                                        class="min-w-0 max-w-full cursor-pointer truncate text-start text-[13px] font-semibold leading-4 text-primary hover:underline"
                                                                        onclick={() => goto(`/album/${albumId}`)}
                                                                >
                                                                        {now.title}
                                                                </button>
                                                        {:else}
                                                                <span class="min-w-0 max-w-full truncate text-[13px] font-semibold leading-4 text-primary">{now.title}</span>
                                                        {/if}
                                                </div>
                                        {:else}
                                                <span class="min-w-0 max-w-full truncate text-[13px] font-semibold leading-4 text-primary"></span>
                                        {/if}
                                        {#if now?.artists}
                                                <span class="truncate text-[12px] font-medium leading-[15px] text-secondary">{now.artists}</span>
                                        {/if}
                                </div>
                                <!-- Elapsed time as a 2px line along the LCD's bottom. Hovering shows the times;
                                     pressing or dragging on the line seeks on release. -->
                                <div
                                        bind:this={scrubberEl}
                                        data-scrubber
                                        data-dragging={dragging !== null ? '' : undefined}
                                        class="group/scrub absolute inset-x-0 bottom-0 h-3.5 cursor-pointer touch-none"
                                        onpointerdown={onScrubDown}
                                        onpointermove={onScrubMove}
                                        onpointerup={onScrubUp}
                                        onpointercancel={onScrubCancel}
                                >
                                        <span
                                                bind:this={elapsedEl}
                                                class="pointer-events-none absolute bottom-[11px] left-0 text-[11px] font-semibold leading-[13px] tabular-nums text-primary opacity-0 transition-opacity group-hover/scrub:opacity-100 group-data-dragging/scrub:opacity-100"
                                        ></span>
                                        <span
                                                bind:this={remainingEl}
                                                class="pointer-events-none absolute bottom-[11px] right-0 text-[11px] font-semibold leading-[13px] tabular-nums text-primary opacity-0 transition-opacity group-hover/scrub:opacity-100 group-data-dragging/scrub:opacity-100"
                                        ></span>
                                        <div
                                                class="absolute inset-x-0 bottom-1.5 h-0.5 overflow-hidden rounded-[1px] bg-foreground/15 transition-[bottom,height] duration-100 ease-in group-hover/scrub:bottom-[5px] group-hover/scrub:h-1 group-data-dragging/scrub:bottom-[5px] group-data-dragging/scrub:h-1"
                                                aria-hidden="true"
                                        >
                                                <div
                                                        bind:this={barEl}
                                                        class="relative size-full origin-left bg-secondary [--progress:0]"
                                                        style="transform: scaleX(var(--progress))"
                                                >
                                                </div>
                                        </div>
                                </div>
                        </div>
                </div>

                <!-- Actions: favourite, lyrics, queue, comments and the volume, which slides in over the
                     others while it's open (they fade out — the original's fadeForVolume). -->
                <div
                        class="group/actions relative flex items-center justify-end gap-[9px]"
                        data-volume-open={volumeOpen || undefined}
                        onpointerleave={() => (volumeOpen = false)}
                >
                        <button
                                class="grid h-7 w-6 cursor-pointer place-items-center rounded-[4px] text-primary transition-opacity group-data-[volume-open]/actions:pointer-events-none group-data-[volume-open]/actions:opacity-0 aria-pressed:text-key"
                                aria-label={t('common.like')}
                                aria-pressed={rating === 'like'}
                                onclick={toggleLike}
                        >
                                {#if rating === 'like'}
                                        <svg width="18" height="18" viewBox="0 0 64 64" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M13.559 60.051c1.102.86 2.5.565 4.166-.645l14.218-10.455L46.19 59.406c1.666 1.21 3.037 1.505 4.166.645 1.102-.833 1.344-2.204.672-4.166l-5.618-16.718 14.353-10.32c1.666-1.183 2.338-2.42 1.908-3.764-.43-1.29-1.693-1.935-3.763-1.908l-17.605.108-5.348-16.8C34.308 4.496 33.34 3.5 31.944 3.5c-1.372 0-2.34.995-2.984 2.984L23.61 23.283l-17.605-.108c-2.07-.027-3.333.618-3.763 1.908-.457 1.344.242 2.58 1.909 3.763l14.352 10.321-5.617 16.718c-.672 1.962-.43 3.333.672 4.166Z"
                                                />
                                        </svg>
                                {:else}
                                        <svg width="18" height="18" viewBox="0 0 64 64" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M13.559 60.051c1.102.86 2.5.565 4.166-.645l14.218-10.455L46.19 59.406c1.666 1.21 3.037 1.505 4.166.645 1.102-.833 1.344-2.204.672-4.166l-5.618-16.718 14.353-10.32c1.666-1.183 2.338-2.42 1.908-3.764-.43-1.29-1.693-1.935-3.763-1.908l-17.605.108-5.348-16.8C34.308 4.496 33.34 3.5 31.944 3.5c-1.372 0-2.34.995-2.984 2.984L23.61 23.283l-17.605-.108c-2.07-.027-3.333.618-3.763 1.908-.457 1.344.242 2.58 1.909 3.763l14.352 10.321-5.617 16.718c-.672 1.962-.43 3.333.672 4.166Zm3.87-5.321c-.054-.054-.027-.081 0-.242l5.349-15.374c.376-1.049.161-1.882-.78-2.527L8.613 27.341c-.134-.08-.161-.134-.134-.215.027-.08.08-.08.242-.08l16.26.295c1.103.027 1.802-.43 2.151-1.532l4.677-15.562c.027-.162.08-.215.134-.215.08 0 .135.053.162.215l4.676 15.562c.35 1.102 1.048 1.559 2.15 1.532l16.261-.296c.162 0 .216 0 .243.081.027.08-.027.134-.135.215l-13.385 9.246c-.94.645-1.156 1.478-.78 2.527l5.35 15.374c.026.161.053.188 0 .242-.055.08-.135.026-.243-.054l-12.928-9.864c-.86-.672-1.855-.672-2.715 0l-12.928 9.864c-.107.08-.188.134-.242.054Z"
                                                />
                                        </svg>
                                {/if}
                        </button>
                        <!-- Always here: the panel itself says whether this track has lyrics, and from where. -->
                        <button
                                class="grid h-7 w-6 cursor-pointer place-items-center rounded-[4px] text-primary transition-opacity group-data-[volume-open]/actions:pointer-events-none group-data-[volume-open]/actions:opacity-0 aria-pressed:text-key"
                                aria-label={t('player.lyrics')}
                                aria-pressed={lyricsOpen}
                                onclick={onToggleLyrics}
                        >
                                {#if lyricsOpen}
                                        <svg width="18" height="18" viewBox="0 0 64 64" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M17.347 62.821c1.254 0 2.21-.572 3.705-1.933L31.78 51.26h18.443C58.943 51.26 64 46.13 64 37.504V14.956C64 6.33 58.944 1.179 50.223 1.179H13.777C5.057 1.179 0 6.322 0 14.956v22.548C0 46.137 5.27 51.26 13.456 51.26h.994v8.327c0 1.97 1.095 3.235 2.897 3.235zm-.108-39.64c0-3.71 2.79-6.37 6.53-6.37 4.18 0 6.89 3.383 6.89 7.694 0 7.102-5.871 11.086-9.19 11.086-.917 0-1.596-.593-1.596-1.43 0-.742.387-1.242 1.403-1.474 2.4-.587 4.629-2.31 5.53-4.606h-.407c-.823.983-2.108 1.318-3.512 1.318-3.417 0-5.648-2.669-5.648-6.217zm16.387 0c0-3.71 2.77-6.37 6.508-6.37 4.18 0 6.912 3.383 6.912 7.694 0 7.102-5.871 11.086-9.179 11.086-.928 0-1.617-.593-1.617-1.43 0-.742.39-1.242 1.392-1.474 2.436-.587 4.654-2.31 5.551-4.606h-.407c-.823.983-2.108 1.318-3.523 1.318-3.405 0-5.637-2.669-5.637-6.217z"
                                                />
                                        </svg>
                                {:else}
                                        <svg width="18" height="18" viewBox="0 0 64 64" fill="currentColor" aria-hidden="true">
                                                <path
                                                        d="M18.53 62.724c1.764 0 3.115-.81 5.257-2.707l9.816-8.638h16.62c8.72 0 13.777-5.152 13.777-13.777V15.053c0-8.625-5.056-13.777-13.777-13.777H13.777C5.057 1.276 0 6.42 0 15.053v22.549c0 8.633 5.27 13.777 13.456 13.777h1.016v6.793c0 2.812 1.511 4.552 4.057 4.552zm1.57-7.16v-8.11c0-1.81-.805-2.485-2.486-2.485h-3.55c-5.165 0-7.654-2.603-7.654-7.654V15.34c0-5.033 2.489-7.632 7.654-7.632h35.872c5.149 0 7.654 2.599 7.654 7.632v21.975c0 5.051-2.505 7.654-7.654 7.654H33.188c-1.835 0-2.702.33-4.012 1.65zm-2.212-32.177c0 3.398 2.156 5.936 5.388 5.936 1.361 0 2.592-.302 3.372-1.263h.385c-.868 2.231-3 3.845-5.303 4.4-.95.243-1.327.737-1.327 1.425 0 .8.658 1.36 1.51 1.36 3.174 0 8.8-3.775 8.8-10.6 0-4.138-2.602-7.336-6.588-7.336-3.576 0-6.237 2.518-6.237 6.078m15.663 0c0 3.398 2.134 5.936 5.387 5.936 1.34 0 2.593-.302 3.373-1.263h.39c-.865 2.231-3.023 3.845-5.308 4.4-.947.243-1.327.737-1.327 1.425 0 .8.636 1.36 1.51 1.36 3.178 0 8.779-3.775 8.779-10.6 0-4.138-2.577-7.336-6.567-7.336-3.577 0-6.237 2.518-6.237 6.078"
                                                />
                                        </svg>
                                {/if}
                        </button>
                        <button
                                class="grid h-7 w-6 cursor-pointer place-items-center rounded-[4px] text-primary transition-opacity group-data-[volume-open]/actions:pointer-events-none group-data-[volume-open]/actions:opacity-0 aria-pressed:text-key"
                                aria-label={t('player.queue')}
                                aria-pressed={queueOpen}
                                onclick={onToggleQueue}
                        >
                                <svg width="18" height="18" viewBox="0 0 18 18" fill="currentColor" aria-hidden="true">
                                        <path
                                                d="M2.634 5.537a.906.906 0 1 0 0-1.813.906.906 0 1 0 0 1.813m3.192-.325h9.865a.576.576 0 0 0 .585-.578.58.58 0 0 0-.585-.585H5.826a.574.574 0 0 0-.585.585c0 .325.253.578.585.578M2.634 9.906c.506 0 .91-.404.91-.91a.906.906 0 0 0-.91-.91.906.906 0 0 0-.91.91c0 .506.405.91.91.91m3.192-.325h9.865a.582.582 0 1 0 0-1.162H5.826a.57.57 0 0 0-.585.577c0 .325.253.585.585.585m-3.192 4.694a.91.91 0 1 0-.001-1.82.91.91 0 0 0 0 1.82zm3.192-.332h9.865a.576.576 0 0 0 .585-.577.58.58 0 0 0-.585-.585H5.826a.574.574 0 0 0-.585.585c0 .324.253.577.585.577"
                                        />
                                </svg>
                        </button>
                        <button
                                class="grid h-7 w-6 cursor-pointer place-items-center rounded-[4px] text-primary transition-opacity group-data-[volume-open]/actions:pointer-events-none group-data-[volume-open]/actions:opacity-0 aria-pressed:text-key"
                                aria-label={t('player.comments')}
                                aria-pressed={commentsOpen}
                                onclick={onToggleComments}
                        >
                                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                        <path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8z" />
                                </svg>
                        </button>
                        <!-- The volume: the speaker mutes, and hovering it slides the level in where the actions
                             beside it were. Wheel over the control steps the level. -->
                        <span bind:this={volumeWrap} class="relative flex items-center">
                                <input
                                        class="pointer-events-none absolute top-1/2 right-[33px] -mt-2 block h-4 w-24 cursor-pointer appearance-none bg-transparent opacity-0 transition-[opacity,translate] translate-x-2 data-open:pointer-events-auto data-open:translate-x-0 data-open:opacity-100 [&::-moz-range-thumb]:size-3 [&::-moz-range-thumb]:rounded-full [&::-moz-range-thumb]:border-0 [&::-moz-range-thumb]:bg-white [&::-moz-range-thumb]:shadow-[0_0_2px_#0006] [&::-moz-range-track]:h-1 [&::-moz-range-track]:rounded-[2px] [&::-moz-range-track]:bg-[linear-gradient(to_right,var(--bc-fill,#fff)_var(--bc-fill,#fff),var(--bc-track,rgba(255,255,255,0.2))_var(--bc-fill,#fff))] [&::-webkit-slider-runnable-track]:h-1 [&::-webkit-slider-runnable-track]:rounded-[2px] [&::-webkit-slider-runnable-track]:bg-[linear-gradient(to_right,var(--bc-fill,#fff)_var(--bc-fill,#fff),var(--bc-track,rgba(255,255,255,0.2))_var(--bc-fill,#fff))] [&::-webkit-slider-thumb]:-mt-1 [&::-webkit-slider-thumb]:size-3 [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-full [&::-webkit-slider-thumb]:bg-white [&::-webkit-slider-thumb]:shadow-[0_0_2px_#0006]"
                                        data-open={volumeOpen || undefined}
                                        type="range"
                                        min={0}
                                        max={100}
                                        step={1}
                                        value={playback.volume}
                                        aria-label={t('player.volume')}
                                        tabindex={volumeOpen ? 0 : -1}
                                        style="--bc-fill: {volumeLevel * 100}%; --bc-track: color-mix(in oklab, currentColor 20%, transparent); color: var(--foreground)"
                                        oninput={onVolumeInput}
                                        onchange={onVolumeCommit}
                                />
                                <button
                                        class="grid h-7 w-6 cursor-pointer place-items-center rounded-[4px] text-primary aria-pressed:text-key"
                                        aria-label={playback.volume === 0 ? t('player.unmute') : t('player.mute')}
                                        aria-pressed={playback.volume === 0}
                                        onclick={() => toggleMute()}
                                        onpointerenter={openVolume}
                                        onfocus={openVolume}
                                >
                                        <!-- Apple's speaker: the body, then three waves, each hidden below its level. -->
                                        <svg width="20" height="20" viewBox="0 0 64 64" fill="currentColor" aria-hidden="true">
                                                <path
                                                        transform="translate(2,11.149)"
                                                        d="m23.477 39.911c1.4129 0 2.431-1.0389 2.431-2.431v-33.141c0-1.3921-1.0181-2.5349-2.4726-2.5349-1.0181 0-1.7038 0.43634-2.805 1.4752l-9.2046 8.6644c-0.14545 0.12464-0.31166 0.18698-0.51945 0.18698h-6.2126c-2.9297 0-4.5088 1.5999-4.5088 4.7374v8.0411c0 3.1167 1.5791 4.7166 4.5088 4.7166h6.2126c0.20779 0 0.374 0.06234 0.51945 0.18698l9.2046 8.7475c0.99732 0.93501 1.8285 1.3506 2.8466 1.3506z"
                                                />
                                                <path
                                                        transform="translate(2,11.149)"
                                                        d="m34.864 29.959c0.70647 0.49868 1.7246 0.35323 2.3271-0.47787 1.6205-2.1817 2.5971-5.3815 2.5971-8.6436 0-3.2621-0.9766-6.4411-2.5971-8.6436-0.60255-0.83111-1.5999-0.97655-2.3271-0.49868-0.89345 0.62336-1.0181 1.683-0.35319 2.5765 1.2051 1.6207 1.9323 4.0932 1.9323 6.5658 0 2.4726-0.76881 4.9451-1.9531 6.5866-0.62332 0.89345-0.51945 1.9116 0.374 2.5349z"
                                                        opacity={volumeLevel === 0 ? 0.25 : 1}
                                                />
                                                <path
                                                        transform="translate(2,11.149)"
                                                        d="m43.154 35.569c0.81021 0.54023 1.8077 0.33245 2.3894-0.49867 2.7426-3.8231 4.3426-8.9137 4.3426-14.233 0-5.3399-1.5583-10.451-4.3426-14.254-0.60255-0.81034-1.5791-1.0181-2.3894-0.47787-0.78979 0.54021-0.91447 1.5583-0.29106 2.4518 2.2647 3.3245 3.6779 7.6878 3.6779 12.28s-1.3923 8.9969-3.6779 12.28c-0.60255 0.89345-0.49872 1.9116 0.29106 2.4518z"
                                                        opacity={volumeLevel >= 1 / 3 ? 1 : 0.25}
                                                />
                                                <path
                                                        transform="translate(2,11.149)"
                                                        d="m51.527 41.241c0.76894 0.51945 1.7872 0.31166 2.3898-0.54021 3.8438-5.423 6.0255-12.446 6.0255-19.864s-2.2443-14.42-6.0255-19.864c-0.60255-0.87268-1.6209-1.0805-2.3898-0.54021-0.78936 0.56098-0.91404 1.5791-0.31149 2.4518 3.3451 4.9244 5.423 11.241 5.423 17.952 0 6.7113-1.9945 13.132-5.423 17.952-0.60255 0.87268-0.47787 1.8908 0.31149 2.4518z"
                                                        opacity={volumeLevel >= 2 / 3 ? 1 : 0.25}
                                                />
                                        </svg>
                                </button>
                        </span>
                </div>
        </div>
</footer>

<style>
        /* The pill is a dark-on-dark glass card regardless of theme, the way BitChord's player pill
           reads over any page: its own recipe, not the app's card colour. The key colour follows the
           active theme's accent (aria-pressed:text-key above). */
        footer {
                --joe: color-mix(in oklab, var(--foreground) 12%, transparent);
        }
</style>
