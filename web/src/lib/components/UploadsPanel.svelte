<script lang="ts">
	import { flip } from 'svelte/animate';
	import { fly, slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import X from '@lucide/svelte/icons/x';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import Upload from '@lucide/svelte/icons/upload';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import { uploads, isActive, type UploadItem } from '#lib/uploads.svelte.ts';
	import { bytes, count, timeLeft } from '#lib/format.ts';

	interface Props {
		/** Open an Inbox album an upload became. */
		onshow: (id: string) => void;
	}
	let { onshow }: Props = $props();

	let collapsed = $state(false);
	/** The clock for retry countdowns, ticking while something waits to retry. */
	let now = $state(Date.now());
	$effect(() => {
		if (!uploads.items.some((i) => i.phase === 'retrying' && i.retryAt)) return;
		now = Date.now();
		const timer = setInterval(() => (now = Date.now()), 500);
		return () => clearInterval(timer);
	});

	let active = $derived(uploads.items.filter((i) => isActive(i.phase)));
	let rate = $derived(active.reduce((n, i) => n + i.rate, 0));
	let summary = $derived.by(() => {
		if (!active.length) {
			const failed = uploads.items.filter((i) => i.phase === 'failed').length;
			return failed ? `${count(failed)} failed` : 'Done';
		}
		const left = active.reduce((n, i) => n + i.size - i.received, 0);
		const speed = rate >= 1000 ? ` · ${bytes(rate)}/s` : '';
		return `${bytes(left)} to go${speed}`;
	});

	const name = (item: UploadItem) => item.folder || 'Upload';
	const files = (item: UploadItem) =>
		`${count(item.files)} ${item.files === 1 ? 'file' : 'files'}, ${bytes(item.size)}`;

	function line(item: UploadItem): string {
		switch (item.phase) {
			case 'queued':
				return `Waiting · ${files(item)}`;
			case 'starting':
				return `Starting · ${files(item)}`;
			case 'sending': {
				const sent = `${bytes(item.received)} of ${bytes(item.size)}`;
				if (item.rate < 1000) return sent;
				const left = timeLeft((item.size - item.received) / item.rate);
				return `${sent} · ${bytes(item.rate)}/s · ${left} left`;
			}
			case 'retrying':
				return item.retryAt
					? `Can’t reach coversyncd. Trying again in ${Math.max(1, Math.ceil((item.retryAt - now) / 1000))} s`
					: 'Reconnecting…';
			case 'finishing':
				return 'Moving it into the uploads folder…';
			case 'done':
				if (item.albums.length > 1) return `${count(item.albums.length)} albums in the Inbox`;
				if (item.albums.length) return 'In the Inbox';
				return item.scanned
					? 'Uploaded; no albums were found in it'
					: 'Uploaded · adding to the Inbox…';
			case 'failed':
				return item.error ?? 'The upload failed.';
			case 'cancelled':
				return 'Cancelled';
		}
	}
</script>

{#if uploads.items.length}
	<section
		class="panel"
		aria-label="Uploads"
		transition:fly={{ y: 24, duration: 320, easing: cubicOut }}
	>
		<header>
			<span class="lead" aria-hidden="true"><Upload size={15} strokeWidth={2.2} /></span>
			<h2>Uploads</h2>
			<span class="summary tabular" aria-live="polite">{summary}</span>
			<button
				class="icon-btn small"
				class:turned={collapsed}
				onclick={() => (collapsed = !collapsed)}
				aria-expanded={!collapsed}
				aria-label={collapsed ? 'Show uploads' : 'Hide uploads'}
				title={collapsed ? 'Show uploads' : 'Hide uploads'}
			>
				<ChevronDown size={16} />
			</button>
			{#if !uploads.busy}
				<button
					class="icon-btn small"
					onclick={() => uploads.clear()}
					aria-label="Close uploads"
					title="Close"
				>
					<X size={15} />
				</button>
			{/if}
		</header>
		{#if !collapsed}
			<ul transition:slide={{ duration: 220 }}>
				{#each uploads.items as item (item.key)}
					<li class={item.phase} animate:flip={{ duration: 260 }} out:slide={{ duration: 200 }}>
						<div class="top">
							{#if item.phase === 'done'}
								<span class="mark ok"><CircleCheck size={15} strokeWidth={2.2} /></span>
							{:else if item.phase === 'failed'}
								<span class="mark bad"><TriangleAlert size={15} strokeWidth={2.2} /></span>
							{/if}
							<span class="name" title={name(item)}>{name(item)}</span>
							{#if isActive(item.phase) && item.phase !== 'finishing'}
								<button
									class="icon-btn small"
									onclick={() => uploads.cancel(item.key)}
									aria-label="Cancel uploading {name(item)}"
									title="Cancel"
								>
									<X size={15} />
								</button>
							{:else if !isActive(item.phase)}
								<button
									class="icon-btn small"
									onclick={() => uploads.dismiss(item.key)}
									aria-label="Dismiss {name(item)}"
									title="Dismiss"
								>
									<X size={15} />
								</button>
							{/if}
						</div>
						{#if isActive(item.phase)}
							<div
								class="bar"
								class:pulse={item.phase === 'finishing' || item.phase === 'starting'}
								class:paused={item.phase === 'retrying'}
								role="progressbar"
								aria-label="Uploading {name(item)}"
								aria-valuemin={0}
								aria-valuemax={item.size}
								aria-valuenow={item.received}
							>
								<span style:--p={item.size ? item.received / item.size : 1}></span>
							</div>
						{/if}
						<div class="line">
							<span
								class="text tabular"
								title={item.phase === 'retrying' ? (item.error ?? undefined) : undefined}
								>{line(item)}</span
							>
							{#if item.phase === 'retrying'}
								<button class="act" onclick={() => uploads.retry(item.key)}>Retry now</button>
							{:else if item.phase === 'failed'}
								<button class="act" onclick={() => uploads.retry(item.key)}>Try again</button>
							{:else if item.phase === 'done' && item.albums.length}
								<button class="act" onclick={() => onshow(item.albums[0])}>Show</button>
							{/if}
						</div>
						{#if item.skipped.length}
							<div
								class="skipped"
								title="Hidden files, and files that aren’t music, images, cue sheets or rip logs, aren’t uploaded:
{item.skipped.slice(0, 30).join('\n')}{item.skipped.length > 30 ? '\n…' : ''}"
							>
								{count(item.skipped.length)}
								{item.skipped.length === 1 ? 'file' : 'files'} left out
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	</section>
{/if}

<style>
	.panel {
		position: fixed;
		right: 20px;
		/* Above the player, when it's showing. */
		bottom: calc(var(--dock, 0px) + 20px);
		z-index: 45;
		width: 380px;
		max-width: calc(100vw - 32px);
		display: grid;
		grid-template-rows: auto minmax(0, 1fr);
		max-height: min(460px, calc(100dvh - var(--topbar-h) - var(--dock, 0px) - 48px));
		border-radius: var(--radius);
		background: color-mix(in oklab, var(--bg-raised) 92%, transparent);
		backdrop-filter: blur(16px);
		border: 1px solid var(--border-strong);
		box-shadow: var(--shadow);
		font-size: 13.5px;
		overflow: hidden;
	}
	header {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 8px 8px 8px 14px;
	}
	.lead {
		display: inline-grid;
		color: var(--accent);
	}
	h2 {
		margin: 0;
		font-size: 14px;
		font-weight: 650;
	}
	.summary {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-3);
		font-size: 12.5px;
	}
	.icon-btn.small {
		flex: none;
		width: 28px;
		height: 28px;
		color: var(--text-3);
	}
	.icon-btn.small:hover {
		color: var(--text);
	}
	.icon-btn.small :global(svg) {
		transition: rotate 220ms var(--ease-out);
	}
	.turned :global(svg) {
		rotate: 180deg;
	}
	ul {
		margin: 0;
		padding: 0 6px 6px;
		list-style: none;
		overflow-y: auto;
		border-top: 1px solid var(--border);
	}
	li {
		display: grid;
		gap: 6px;
		padding: 10px 8px 10px 8px;
	}
	li + li {
		border-top: 1px solid var(--border);
	}
	.top {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 28px;
	}
	.mark {
		display: inline-grid;
		flex: none;
	}
	.mark.ok {
		color: var(--success);
	}
	.mark.bad {
		color: var(--danger);
	}
	.name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-weight: 560;
	}
	.bar {
		height: 6px;
		border-radius: 999px;
		background: var(--surface-hover);
		overflow: hidden;
	}
	.bar span {
		display: block;
		height: 100%;
		width: 100%;
		border-radius: inherit;
		background: var(--accent);
		transform-origin: left;
		transform: scaleX(var(--p));
		transition: transform 260ms linear;
	}
	.bar.paused span {
		background: var(--text-3);
	}
	.bar.pulse span {
		animation: pulse 1.2s ease-in-out infinite alternate;
	}
	@keyframes pulse {
		from {
			opacity: 1;
		}
		to {
			opacity: 0.45;
		}
	}
	.line {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 24px;
		color: var(--text-2);
		font-size: 12.5px;
	}
	.text {
		flex: 1;
		min-width: 0;
	}
	.failed .text {
		color: var(--danger);
	}
	.act {
		flex: none;
		padding: 3px 10px;
		border-radius: 999px;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 14%, transparent);
		white-space: nowrap;
	}
	.act:hover {
		background: color-mix(in oklab, var(--accent) 22%, transparent);
	}
	.skipped {
		width: fit-content;
		color: var(--warn-text);
		font-size: 12px;
		cursor: help;
	}
	@media (max-width: 600px) {
		.panel {
			right: 12px;
			left: 12px;
			width: auto;
			max-width: none;
			bottom: calc(var(--dock, 0px) + 12px);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.bar span {
			transition: none;
		}
		.bar.pulse span {
			animation: none;
		}
	}
</style>
