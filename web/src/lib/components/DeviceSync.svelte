<script lang="ts">
	import { fade, slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import FolderSearch from '@lucide/svelte/icons/folder-search';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleX from '@lucide/svelte/icons/circle-x';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import Clock from '@lucide/svelte/icons/clock';
	import type {
		DeviceSyncState,
		FileCount,
		SyncFailure,
		SyncReason,
		SyncResult
	} from '#lib/api.ts';
	import { ago, devices } from '#lib/devices.svelte.ts';
	import { bytes, count } from '#lib/format.ts';
	import ShowAll from './ShowAll.svelte';

	interface Props {
		/** The device's id. */
		id: number;
		sync: DeviceSyncState;
		/** A password login, which may ask things of the device's sync. */
		manage: boolean;
	}
	let { id, sync, manage }: Props = $props();

	/** Removing the files coversync didn't put there, waiting for its confirmation. */
	let confirmRemove = $state(false);

	const files = (n: number) => `${count(n)} ${n === 1 ? 'file' : 'files'}`;
	/** "12 files (1.2 GB)", the size only when there is one. */
	const sized = (c: FileCount) =>
		c.bytes ? `${files(c.files)} (${bytes(c.bytes)})` : files(c.files);

	const ENDED: Record<SyncResult, string> = {
		complete: 'Synced',
		partial: 'Sync stopped part way',
		cancelled: 'Sync cancelled',
		failed: 'Sync failed'
	};
	const STARTED: Record<SyncReason, string | null> = {
		user: 'on request',
		event: 'after a change',
		timer: 'on a regular check',
		wake: 'when it woke',
		other: null
	};
	const WHY: Record<string, string> = {
		no_space: 'No space left',
		path_too_long: 'Path too long for the device',
		protected: 'A name the device keeps for itself',
		changed: 'Changed on the device',
		gone: 'Changed in the Library meanwhile',
		io: "Couldn't be written"
	};

	function why(f: SyncFailure) {
		const known = Object.hasOwn(WHY, f.error) ? WHY[f.error] : null;
		if (!known) return f.detail ?? f.error;
		return f.detail ? `${known}: ${f.detail}` : known;
	}

	/** A sync under way: how far it has got, in words and as a share of the whole. */
	let progress = $derived.by(() => {
		const r = sync.running;
		if (!r) return null;
		const t = r.totals;
		if (!t)
			return { text: r.reconciling ? 'looking through its files' : 'getting ready', share: null };
		const total = t.put + t.move + t.delete;
		if (!total) return { text: null, share: null };
		const done = Math.min(r.progress.files, total);
		const copied = Math.min(r.progress.bytes, t.put_bytes);
		const parts = [`${count(done)} of ${files(total)}`];
		if (t.put_bytes) parts.push(`${bytes(copied)} of ${bytes(t.put_bytes)}`);
		if (r.progress.rate) parts.push(`${bytes(r.progress.rate)}/s`);
		return { text: parts.join(' · '), share: t.put_bytes ? copied / t.put_bytes : done / total };
	});

	let upToDate = $derived(
		!sync.running &&
			!sync.no_library &&
			sync.reconciled &&
			!sync.pending.files &&
			!sync.preparing &&
			!sync.too_long &&
			!sync.short_bytes &&
			!sync.failed.count
	);

	let busy = $derived(devices.syncBusy[id]);
	/** Failed files listed at once; the rest behind Show all. */
	const FIRST = 3;

	async function removeOther() {
		if (await devices.syncAction(id, 'remove_other')) confirmRemove = false;
	}
</script>

<div class="sync" transition:slide={{ duration: 220, easing: cubicOut }}>
	<div class="head">
		<p class="status">
			{#if sync.running}
				<span class="icon accent spin"><RefreshCw size={14} /></span>
				<b>Syncing</b>
				{#if progress?.text}<span class="muted tabular">· {progress.text}</span>{/if}
			{:else if sync.last}
				{@const { result, files: done, bytes: copied } = sync.last.report}
				{@const started = STARTED[sync.last.reason]}
				<span
					class="icon"
					class:ok={result === 'complete'}
					class:warn={result === 'partial'}
					class:bad={result === 'failed'}
				>
					{#if result === 'complete'}<CircleCheck size={14} />
					{:else if result === 'partial'}<TriangleAlert size={14} />
					{:else}<CircleX size={14} />{/if}
				</span>
				<b>{ENDED[result]} {ago(sync.last.ended_at)}{started ? `, ${started}` : ''}</b>
				{#if done}<span class="muted tabular">· {sized({ files: done, bytes: copied })}</span>{/if}
			{:else}
				<span class="icon"><Clock size={14} /></span>
				<!-- Its folder has been looked through in a sync that never reported its end. -->
				<b>{sync.reconciled ? 'No sync has finished yet' : 'Never synced'}</b>
			{/if}
			{#if upToDate}<span class="tag ok">Up to date</span>{/if}
		</p>
		{#if manage}
			<div class="actions">
				<button
					class="btn sm"
					onclick={() => devices.syncAction(id, 'sync')}
					disabled={!!busy || !!sync.running || sync.sync_requested || sync.no_library}
				>
					{#if busy === 'sync'}<span class="spin"><LoaderCircle size={14} /></span>
					{:else}<RefreshCw size={14} />{/if}
					Sync now
				</button>
				<button
					class="btn sm"
					onclick={() => devices.syncAction(id, 'check')}
					disabled={!!busy || sync.check_requested}
					title="Look through its whole folder at its next sync, for files changed or removed on the device."
				>
					{#if busy === 'check'}<span class="spin"><LoaderCircle size={14} /></span>
					{:else}<FolderSearch size={14} />{/if}
					Check files
				</button>
			</div>
		{/if}
	</div>

	{#if progress?.share != null}
		<div
			class="bar"
			role="progressbar"
			aria-label="Syncing"
			aria-valuemin={0}
			aria-valuemax={1}
			aria-valuenow={progress.share}
		>
			<span style:--p={progress.share}></span>
		</div>
	{/if}
	{#if !sync.running && sync.last?.report.message}
		<p class="line muted">{sync.last.report.message}</p>
	{/if}
	{#if sync.sync_requested}
		<p class="line muted" in:fade={{ duration: 150 }}>
			<Clock size={14} /> Waiting for the device to start syncing.
		</p>
	{/if}
	{#if sync.check_requested}
		<p class="line muted" in:fade={{ duration: 150 }}>
			<Clock size={14} /> Its whole folder will be checked at its next sync.
		</p>
	{/if}

	{#if sync.no_library}
		<p class="line warn">
			<TriangleAlert size={14} /> The Library folder isn't available, so there's nothing to sync.
		</p>
	{:else}
		{#if sync.pending.files}
			<p class="line tabular">Still to sync: {sized(sync.pending)}</p>
		{/if}
		{#if sync.preparing}
			<p class="line muted tabular">
				{files(sync.preparing)}
				{sync.preparing === 1 ? "isn't" : "aren't"} ready to send yet.
			</p>
		{/if}
		{#if sync.too_long}
			<p class="line warn">
				<TriangleAlert size={14} />
				<span>
					{files(sync.too_long)}
					{sync.too_long === 1 ? 'has a path' : 'have paths'} too long for this device. A shorter folder
					structure would fit {sync.too_long === 1 ? 'it' : 'them'}.
				</span>
			</p>
		{/if}
		{#if sync.short_bytes}
			<p class="line warn">
				<TriangleAlert size={14} /> It needs {bytes(sync.short_bytes)} more space to hold everything.
			</p>
		{/if}
	{/if}

	{#if sync.failed.count}
		<div class="group">
			<div class="row">
				<p class="line bad">
					<CircleX size={14} />
					<span>
						<b>{files(sync.failed.count)} couldn't be synced.</b>
						<span class="muted">Each is skipped until it changes or is retried.</span>
					</span>
				</p>
				{#if manage}
					<button class="btn sm" onclick={() => devices.syncAction(id, 'retry')} disabled={!!busy}>
						{#if busy === 'retry'}<span class="spin"><LoaderCircle size={14} /></span>
						{:else}<RotateCcw size={14} />{/if}
						Retry
					</button>
				{/if}
			</div>
			{#snippet failures(list: SyncFailure[])}
				<ul class="failures">
					{#each list as f (`${f.path}\n${f.hash}`)}
						<li><span class="path">{f.path}</span> <span class="muted">{why(f)}</span></li>
					{/each}
				</ul>
			{/snippet}
			{@render failures(sync.failed.files.slice(0, FIRST))}
			{#if sync.failed.count > FIRST}
				<ShowAll label="Show all" count={sync.failed.count - FIRST}>
					{@render failures(sync.failed.files.slice(FIRST))}
					{#if sync.failed.count > sync.failed.files.length}
						<p class="line muted tabular">
							And {count(sync.failed.count - sync.failed.files.length)} more.
						</p>
					{/if}
				</ShowAll>
			{/if}
		</div>
	{/if}

	{#if sync.remove_other && sync.other.files}
		<div class="row">
			<p class="line">
				Its folder has {sized(sync.other)} that coversync didn't put there: they're removed at its next
				sync.
			</p>
			{#if manage}
				<button
					class="btn sm"
					onclick={() => devices.syncAction(id, 'keep_other')}
					disabled={!!busy}
				>
					{#if busy === 'keep_other'}<span class="spin"><LoaderCircle size={14} /></span>{/if}
					Keep them
				</button>
			{/if}
		</div>
	{:else if sync.other.files}
		<div class="row">
			<p class="line">
				Its folder has {sized(sync.other)} that coversync didn't put there. They're left as they are.
			</p>
			{#if manage}
				{#if confirmRemove}
					<div class="actions" in:fade={{ duration: 150 }}>
						<span class="muted">Remove them from the device?</span>
						<button class="btn sm" onclick={() => (confirmRemove = false)}>Cancel</button>
						<button class="btn sm danger" onclick={removeOther} disabled={!!busy}>
							{#if busy === 'remove_other'}<span class="spin"><LoaderCircle size={14} /></span>{/if}
							Remove
						</button>
					</div>
				{:else}
					<button class="btn sm" onclick={() => (confirmRemove = true)} disabled={!!busy}>
						Remove them
					</button>
				{/if}
			{/if}
		</div>
	{/if}
</div>

<style>
	.sync {
		grid-column: 2 / -1;
		display: grid;
		gap: 6px;
		padding: 10px 12px;
		border-radius: 12px;
		background: var(--surface);
		font-size: 13px;
		color: var(--text-2);
	}
	.head,
	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 12px;
	}
	.status {
		flex: 1 1 240px;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
		margin: 0;
	}
	.status b {
		font-weight: 600;
		color: var(--text);
	}
	.icon {
		display: inline-grid;
		color: var(--text-3);
	}
	.icon.accent {
		color: var(--accent);
	}
	.icon.ok {
		color: var(--success);
	}
	.icon.warn {
		color: var(--warn);
	}
	.icon.bad {
		color: var(--danger);
	}
	.tag {
		padding: 1px 8px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
	}
	.tag.ok {
		color: var(--success);
		background: color-mix(in oklab, var(--success) 14%, transparent);
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		margin-left: auto;
	}
	.row .line {
		flex: 1 1 240px;
	}
	.line {
		display: flex;
		align-items: flex-start;
		gap: 6px;
		margin: 0;
		overflow-wrap: anywhere;
	}
	.line > :global(svg) {
		flex: none;
		margin-top: 2px;
	}
	.line.warn {
		color: var(--warn-text);
	}
	.line.warn > :global(svg) {
		color: var(--warn);
	}
	.line.bad > :global(svg) {
		color: var(--danger);
	}
	.line b {
		font-weight: 600;
		color: var(--text);
	}
	.muted {
		color: var(--text-3);
	}
	.danger {
		color: var(--danger);
		border-color: color-mix(in oklab, var(--danger) 40%, transparent);
	}
	.group {
		display: grid;
		gap: 4px;
	}
	.failures {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0 0 0 20px;
		list-style: none;
		font-size: 12.5px;
	}
	.failures li {
		overflow-wrap: anywhere;
	}
	.path {
		color: var(--text-2);
	}
	.bar {
		height: 4px;
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
	.spin {
		display: inline-grid;
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}
	@media (max-width: 760px) {
		.sync {
			grid-column: 1 / -1;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.bar span {
			transition: none;
		}
		.spin {
			animation: none;
		}
	}
</style>
