<script lang="ts">
	import Play from '@lucide/svelte/icons/play';
	import Pause from '@lucide/svelte/icons/pause';
	import SkipBack from '@lucide/svelte/icons/skip-back';
	import SkipForward from '@lucide/svelte/icons/skip-forward';
	import Volume2 from '@lucide/svelte/icons/volume-2';
	import VolumeX from '@lucide/svelte/icons/volume-x';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { fly, scale } from 'svelte/transition';
	import { coverUrl } from '#lib/api.ts';
	import { accent } from '#lib/color.ts';
	import { duration, quality } from '#lib/format.ts';
	import { player } from '#lib/player.svelte.ts';

	interface Props {
		onopen: (albumId: string) => void;
	}
	let { onopen }: Props = $props();

	let track = $derived(player.current);
	let progress = $derived(player.duration ? player.time / player.duration : 0);
	let scrubbing = $state<number | null>(null);
	let bar = $state<HTMLElement>();

	function ratioAt(e: PointerEvent) {
		const r = bar!.getBoundingClientRect();
		return Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
	}
	function down(e: PointerEvent) {
		bar!.setPointerCapture(e.pointerId);
		scrubbing = ratioAt(e);
	}
	function move(e: PointerEvent) {
		if (scrubbing !== null) scrubbing = ratioAt(e);
	}
	function up() {
		if (scrubbing !== null) player.seek(scrubbing * player.duration);
		scrubbing = null;
	}
	function key(e: KeyboardEvent) {
		if (e.key === 'ArrowRight') player.seek(player.time + 5);
		else if (e.key === 'ArrowLeft') player.seek(player.time - 5);
		else return;
		e.preventDefault();
	}
	let shown = $derived(scrubbing ?? progress);
</script>

{#if track}
	<div
		class="player"
		style:--accent={accent(player.album?.color ?? null)}
		transition:fly={{ y: 100, duration: 520, opacity: 1 }}
		role="region"
		aria-label="Player"
	>
		<button class="np" onclick={() => onopen(track.album_id)} aria-label="Open album">
			{#key track.album_id}
				<span class="art" in:scale={{ start: 0.85, duration: 380 }}>
					{#if player.album?.has_cover}
						<img src={coverUrl(track.album_id, 256)} alt="" />
					{/if}
				</span>
			{/key}
			<span class="np-text">
				{#key track.id}
					<span class="np-title" in:fly={{ y: 8, duration: 320 }}>{track.title}</span>
				{/key}
				<span class="np-artist">{track.artist}</span>
			</span>
		</button>

		<div class="center">
			<div class="controls">
				<button class="icon-btn" onclick={() => player.prev()} aria-label="Previous">
					<SkipBack size={18} fill="currentColor" />
				</button>
				<button
					class="main"
					onclick={() => player.toggle()}
					aria-label={player.playing ? 'Pause' : 'Play'}
				>
					{#if player.buffering}
						<span class="spin"><LoaderCircle size={20} /></span>
					{:else if player.playing}
						<span in:scale={{ duration: 220, start: 0.5 }}
							><Pause size={20} fill="currentColor" strokeWidth={0} /></span
						>
					{:else}
						<span class="nudge" in:scale={{ duration: 220, start: 0.5 }}
							><Play size={20} fill="currentColor" strokeWidth={0} /></span
						>
					{/if}
				</button>
				<button class="icon-btn" onclick={() => player.next()} aria-label="Next">
					<SkipForward size={18} fill="currentColor" />
				</button>
			</div>
			<div class="timeline">
				<span class="t tabular"
					>{duration((scrubbing !== null ? scrubbing * player.duration : player.time) * 1000)}</span
				>
				<div
					class="bar"
					class:active={scrubbing !== null}
					bind:this={bar}
					onpointerdown={down}
					onpointermove={move}
					onpointerup={up}
					onpointercancel={up}
					onkeydown={key}
					role="slider"
					tabindex="0"
					aria-label="Seek"
					aria-valuemin={0}
					aria-valuemax={Math.round(player.duration)}
					aria-valuenow={Math.round(player.time)}
					aria-valuetext={duration(player.time * 1000)}
				>
					<span class="track"><span class="fill" style:transform="scaleX({shown})"></span></span>
					<span class="knob" style:left="{shown * 100}%"></span>
				</div>
				<span class="t tabular">{duration(player.duration * 1000)}</span>
			</div>
		</div>

		<div class="side">
			<span class="quality">{quality(track)}</span>
			<button
				class="icon-btn"
				onclick={() => player.toggleMute()}
				aria-label={player.muted ? 'Unmute' : 'Mute'}
			>
				{#if player.muted || player.volume === 0}<VolumeX size={18} />{:else}<Volume2
						size={18}
					/>{/if}
			</button>
			<input
				class="volume"
				type="range"
				min="0"
				max="1"
				step="0.01"
				value={player.muted ? 0 : player.volume}
				oninput={(e) => player.setVolume(+e.currentTarget.value)}
				style:--v={player.muted ? 0 : player.volume}
				aria-label="Volume"
			/>
		</div>
	</div>
{/if}

<style>
	.player {
		position: fixed;
		inset: auto 12px 12px;
		z-index: 40;
		height: var(--player-h);
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(320px, 2fr) minmax(0, 1fr);
		align-items: center;
		gap: 20px;
		padding: 0 18px 0 12px;
		border-radius: 20px;
		background: color-mix(in oklab, var(--bg-raised) 82%, transparent);
		backdrop-filter: saturate(1.8) blur(28px);
		border: 1px solid var(--border);
		box-shadow: var(--shadow);
	}
	.np {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
		text-align: left;
		border-radius: 12px;
		padding: 6px;
		transition: background 160ms;
	}
	.np:hover {
		background: var(--surface);
	}
	.art {
		flex: none;
		width: 56px;
		height: 56px;
		border-radius: 10px;
		overflow: hidden;
		background: var(--surface-hover);
		box-shadow: 0 6px 16px -6px rgb(0 0 0 / 0.6);
	}
	.art img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
	}
	.np-text {
		display: grid;
		min-width: 0;
	}
	.np-title {
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.np-artist {
		font-size: 13px;
		color: var(--text-2);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.center {
		display: grid;
		gap: 4px;
	}
	.controls {
		display: flex;
		justify-content: center;
		align-items: center;
		gap: 14px;
	}
	.main {
		display: grid;
		place-items: center;
		width: 40px;
		height: 40px;
		border-radius: 999px;
		background: var(--text);
		color: var(--bg);
		transition: transform 220ms var(--ease-spring);
	}
	.main:hover {
		transform: scale(1.07);
	}
	.main:active {
		transform: scale(0.94);
	}
	.main > span {
		display: grid;
	}
	.nudge {
		padding-left: 2px;
	}
	.spin {
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}

	.timeline {
		display: grid;
		grid-template-columns: 44px 1fr 44px;
		align-items: center;
		gap: 10px;
	}
	.t {
		font-size: 11.5px;
		color: var(--text-3);
		text-align: center;
	}
	.bar {
		position: relative;
		height: 16px;
		display: flex;
		align-items: center;
		cursor: pointer;
		touch-action: none;
	}
	.track {
		width: 100%;
		height: 4px;
		border-radius: 999px;
		background: var(--border-strong);
		overflow: hidden;
		transition: height 180ms var(--ease-out);
	}
	.fill {
		display: block;
		height: 100%;
		background: var(--accent);
		transform-origin: left;
	}
	.knob {
		position: absolute;
		top: 50%;
		width: 12px;
		height: 12px;
		margin: -6px 0 0 -6px;
		border-radius: 999px;
		background: var(--text);
		box-shadow: 0 2px 6px rgb(0 0 0 / 0.5);
		transform: scale(0);
		transition: transform 200ms var(--ease-spring);
	}
	.bar:hover .track,
	.bar.active .track {
		height: 6px;
	}
	.bar:hover .knob,
	.bar.active .knob,
	.bar:focus-visible .knob {
		transform: scale(1);
	}

	.side {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: 6px;
	}
	.quality {
		font-size: 11.5px;
		color: var(--text-3);
		margin-right: 8px;
		white-space: nowrap;
	}
	.volume {
		width: 96px;
		height: 16px;
		appearance: none;
		background: transparent;
		cursor: pointer;
	}
	.volume::-moz-range-track {
		height: 4px;
		border-radius: 999px;
		background: var(--border-strong);
	}
	.volume::-moz-range-progress {
		height: 4px;
		border-radius: 999px;
		background: var(--text-2);
	}
	.volume::-moz-range-thumb {
		width: 12px;
		height: 12px;
		border: 0;
		border-radius: 999px;
		background: var(--text);
	}
	.volume::-webkit-slider-runnable-track {
		height: 4px;
		border-radius: 999px;
		background: linear-gradient(
			to right,
			var(--text-2) calc(var(--v) * 100%),
			var(--border-strong) 0
		);
	}
	.volume::-webkit-slider-thumb {
		appearance: none;
		width: 12px;
		height: 12px;
		margin-top: -4px;
		border-radius: 999px;
		background: var(--text);
	}

	@media (max-width: 800px) {
		.player {
			grid-template-columns: minmax(0, 1fr) auto;
			padding: 8px 12px;
			gap: 8px;
		}
		.center {
			grid-row: 2;
			grid-column: 1 / -1;
		}
		.side {
			display: none;
		}
	}
</style>
