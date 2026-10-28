<script lang="ts">
	import { untrack } from 'svelte';
	import { cubicOut } from 'svelte/easing';
	import { Tween } from 'svelte/motion';
	import { fly, slide } from 'svelte/transition';
	import ShieldCheck from '@lucide/svelte/icons/shield-check';
	import HardDrive from '@lucide/svelte/icons/hard-drive';
	import FolderTree from '@lucide/svelte/icons/folder-tree';
	import FolderField from './FolderField.svelte';
	import StructureField from './StructureField.svelte';
	import ShowAll from './ShowAll.svelte';
	import Tags from '@lucide/svelte/icons/tags';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import Sparkles from '@lucide/svelte/icons/sparkles';
	import Eraser from '@lucide/svelte/icons/eraser';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import ImageIcon from '@lucide/svelte/icons/image';
	import FileImage from '@lucide/svelte/icons/file-image';
	import FileMusic from '@lucide/svelte/icons/file-music';
	import Toggle from './Toggle.svelte';
	import type { ArtPlacement, TagKind } from '#lib/api.ts';
	import { count, fold } from '#lib/format.ts';
	import { setup } from '#lib/setup.svelte.ts';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';

	interface Props {
		/** Filter text from the top bar's search field. */
		query: string;
		scanning: boolean;
		/** Bumped when an Inbox scan finishes, to refresh coverage. */
		generation: number;
		back: Back;
	}
	let { query, scanning, generation, back }: Props = $props();

	setup.load();
	$effect(() => {
		generation;
		untrack(() => setup.loadCoverage());
	});

	let coverage = $derived(new Map(setup.coverage?.kinds.map((k, i) => [k.id, { ...k, i }]) ?? []));
	let albumSets = $derived(
		Object.values(setup.coverage?.album_kinds ?? {}).map((kinds) => new Set(kinds))
	);
	/** Inbox albums whose every track has every required tag (after autofill). */
	let meeting = $derived.by(() => {
		if (!setup.coverage) return null;
		const required = [...setup.required].map((id) => coverage.get(id)?.i ?? -1);
		return albumSets.filter((s) => required.every((i) => s.has(i))).length;
	});
	const shown = new Tween(0, { duration: 600, easing: cubicOut });
	$effect(() => {
		if (meeting !== null) shown.target = meeting;
	});
	let ratio = $derived(setup.coverage?.albums ? (meeting ?? 0) / setup.coverage.albums : 0);

	function share(kind: TagKind) {
		const c = coverage.get(kind.id);
		const total = setup.coverage?.tracks ?? 0;
		return c && total ? c.tracks / total : 0;
	}
	function shareLabel(p: number) {
		if (p === 0) return 'none';
		if (p < 0.01) return '<1%';
		return `${Math.round(p * 100)}%`;
	}
	function shareTitle(kind: TagKind) {
		const c = coverage.get(kind.id);
		if (!c || !setup.coverage) return undefined;
		return `${count(c.tracks)} of ${count(setup.coverage.tracks)} Inbox tracks · complete on ${count(c.albums)} albums`;
	}

	let terms = $derived(fold(query).split(/\s+/).filter(Boolean));
	let groups = $derived(
		setup.groups
			.map((g) => ({
				...g,
				kinds: setup.kinds.filter(
					(k) => k.group === g.id && terms.every((t) => fold(`${k.label} ${k.id}`).includes(t))
				)
			}))
			.filter((g) => g.kinds.length)
	);

	/** Everything but Core sits behind "Show all tags". */
	let otherGroups = $derived(groups.filter((g) => g.id !== 'core'));
	/** Required tags outside Core, flagged on the closed "Show all tags". */
	let hiddenRequired = $derived(
		setup.kinds.filter((k) => k.group !== 'core' && setup.required.has(k.id)).length
	);

	// Core and any group with required tags start open; after that, the user decides.
	let open = $state<Record<string, boolean>>({});
	let seeded = false;
	$effect(() => {
		if (!setup.loaded || seeded) return;
		seeded = true;
		for (const g of setup.groups)
			open[g.id] =
				g.id === 'core' || setup.kinds.some((k) => k.group === g.id && setup.required.has(k.id));
	});
	const isOpen = (id: string) => terms.length > 0 || !!open[id];

	/** The cover file name as it will be saved (the daemon trims the same way). */
	let artName = $derived(setup.art.file_name.trim().replace(/\.(jpe?g|png|webp)$/i, '') || 'cover');
	let artNameBad = $derived(/[/\\]/.test(artName) || artName.startsWith('.'));
	const placements: { id: ArtPlacement; label: string; file: boolean; embedded: boolean }[] = [
		{ id: 'file', label: 'Cover file', file: true, embedded: false },
		{ id: 'embedded', label: 'Embedded', file: false, embedded: true },
		{ id: 'both', label: 'Both', file: true, embedded: true }
	];
	function placementText(id: ArtPlacement) {
		if (id === 'file')
			return `${artName}.jpg beside the tracks. A folder image is renamed; art found only inside the files is saved out.`;
		if (id === 'embedded')
			return "Inside every audio file. Tracks without art get the album's cover; no image file is written.";
		return `${artName}.jpg beside the tracks, and the cover embedded in every file.`;
	}
	let withoutArt = $derived(
		setup.coverage
			? setup.coverage.without_art
				? `${count(setup.coverage.without_art)} of ${count(setup.coverage.albums)} Inbox albums have none.`
				: 'Every Inbox album has some.'
			: ''
	);

	function groupState(kinds: TagKind[]) {
		const on = kinds.filter((k) => setup.required.has(k.id)).length;
		return { on, all: on === kinds.length, some: on > 0 && on < kinds.length };
	}
</script>

<SetupPane
	{back}
	title="Library setup"
	description="Where coversync keeps the files it manages, how it lays them out, and the rules they follow."
>
	{#if setup.failed}
		<div class="notice" role="alert">
			<TriangleAlert size={18} /> Couldn't load the library setup from coversyncd.
			<button class="btn sm" onclick={() => setup.load()}>Retry</button>
		</div>
	{:else if !setup.loaded}
		<div class="card skeleton" aria-busy="true"><span></span><span></span><span></span></div>
	{:else}
		<section class="card" aria-labelledby="location-title" in:fly={{ y: 12, duration: 420 }}>
			<header class="card-head">
				<span class="card-icon"><HardDrive size={20} /></span>
				<div>
					<h2 id="location-title">Library folder</h2>
					<p>Imported albums are written here. It can't be inside the Inbox folder.</p>
				</div>
			</header>
			<div class="card-body">
				<FolderField
					id="library-folder"
					label="Folder"
					placeholder="/data/Library"
					purpose="library"
					bind:value={setup.folder}
				/>
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="structure-title"
			in:fly={{ y: 12, duration: 420, delay: 60 }}
		>
			<header class="card-head">
				<span class="card-icon"><FolderTree size={20} /></span>
				<div>
					<h2 id="structure-title">Folder structure</h2>
					<p>
						Where each file goes inside the library folder. Each / starts a folder, {'{variables}'}
						are filled from tags, and the file extension is added for you.
					</p>
				</div>
			</header>
			<div class="card-body">
				<StructureField
					bind:value={setup.structure}
					folder={setup.folder}
					suggestions={setup.suggestions}
					variables={setup.variables}
					label={(id) => setup.label(id)}
				/>
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="rules-title"
			in:fly={{ y: 12, duration: 420, delay: 120 }}
		>
			<header class="card-head">
				<span class="card-icon"><ShieldCheck size={20} /></span>
				<div>
					<h2 id="rules-title">Rules</h2>
					<p>A file has to pass every rule before coversync takes it into the Library.</p>
				</div>
			</header>

			<div class="rule card-body">
				<div class="rule-head">
					<span class="rule-icon"><Tags size={17} /></span>
					<div class="rule-text">
						<h3>Require tags</h3>
						<p>Each managed file needs a value for every checked tag.</p>
					</div>
					<div class="rule-actions">
						<button class="btn sm" onclick={() => setup.setAll(setup.essentials, true)}>
							<Sparkles size={14} /> Essentials
						</button>
						<button
							class="btn sm"
							onclick={() => setup.setAll([...setup.required], false)}
							disabled={setup.required.size === 0}
						>
							<Eraser size={14} /> Clear
						</button>
					</div>
				</div>

				<div class="impact" aria-live="polite">
					{#if meeting === null}
						<span class="impact-text muted">Checking the Inbox…</span>
					{:else}
						<span class="impact-text">
							<b class="tabular">{count(Math.round(shown.current))}</b>
							of {count(setup.coverage?.albums ?? 0)} Inbox albums
							{setup.required.size
								? 'have every required tag, counting autofill'
								: 'qualify, since no tags are required yet'}
						</span>
						<span class="required tabular">{setup.required.size} required</span>
					{/if}
					<span class="meter"><span style:transform="scaleX({ratio})"></span></span>
					{#if scanning}<span class="impact-note"
							>The Inbox is being scanned; numbers update when it finishes.</span
						>{/if}
				</div>

				{#snippet tagGroup(g: (typeof groups)[number])}
					{@const state = groupState(g.kinds)}
					<div class="group" class:open={isOpen(g.id)}>
						<div class="group-head">
							<button
								class="group-toggle"
								aria-expanded={isOpen(g.id)}
								aria-controls="group-{g.id}"
								onclick={() => (open[g.id] = !isOpen(g.id))}
							>
								<span class="chevron"><ChevronRight size={16} /></span>
								<span class="group-label">{g.label}</span>
								<span class="group-count tabular" class:active={state.on > 0}>
									{state.on ? `${state.on} of ${g.kinds.length}` : g.kinds.length}
								</span>
							</button>
							<label
								class="all"
								title={state.all ? 'Require none in this group' : 'Require all in this group'}
							>
								<input
									type="checkbox"
									checked={state.all}
									{@attach (el: HTMLInputElement) => {
										el.indeterminate = state.some;
									}}
									onchange={(e) =>
										setup.setAll(
											g.kinds.map((k) => k.id),
											e.currentTarget.checked
										)}
									aria-label="Require every {g.label} tag"
								/>
							</label>
						</div>
						{#if isOpen(g.id)}
							<ul
								id="group-{g.id}"
								class="kinds"
								transition:slide={{ duration: 280, easing: cubicOut }}
							>
								{#each g.kinds as k (k.id)}
									{@const p = share(k)}
									<li>
										<label class="kind" class:on={setup.required.has(k.id)}>
											<input
												type="checkbox"
												checked={setup.required.has(k.id)}
												onchange={(e) => setup.toggle(k.id, e.currentTarget.checked)}
											/>
											<span class="kind-label">{k.label}</span>
											<span class="share" title={shareTitle(k)}>
												<span class="share-bar"><i style:width="{p * 100}%"></i></span>
												<span class="share-pct tabular" class:none={p === 0}>{shareLabel(p)}</span>
											</span>
										</label>
									</li>
								{/each}
							</ul>
						{/if}
					</div>
				{/snippet}

				{#each groups.filter((g) => g.id === 'core') as g (g.id)}
					{@render tagGroup(g)}
				{/each}
				{#if otherGroups.length}
					<ShowAll
						label="Show all tags"
						count={otherGroups.reduce((n, g) => n + g.kinds.length, 0)}
						badge={hiddenRequired ? `${hiddenRequired} required` : undefined}
						forceOpen={terms.length > 0}
					>
						{#each otherGroups as g (g.id)}
							{@render tagGroup(g)}
						{/each}
					</ShowAll>
				{/if}
				{#if !groups.length}
					<p class="no-match">No tags match “{query}”.</p>
				{/if}
			</div>

			<div class="rule card-body art-rule">
				<div class="rule-head">
					<span class="rule-icon"><ImageIcon size={17} /></span>
					<div class="rule-text">
						<h3>Album art</h3>
						<p>How managed albums keep their cover. Importing puts it in place for you.</p>
					</div>
				</div>

				<Toggle
					bind:checked={setup.art.required}
					label="Require album art"
					description="Albums with no art need attention, and importing one asks for an image. {withoutArt}"
				/>

				<div class="placements" role="radiogroup" aria-label="Where album art goes">
					{#each placements as p (p.id)}
						<label class="placement" class:on={setup.art.placement === p.id}>
							<input
								type="radio"
								name="art-placement"
								value={p.id}
								bind:group={setup.art.placement}
							/>
							<span class="glyph" aria-hidden="true">
								<span class="audio">
									<FileMusic size={22} strokeWidth={1.6} />
									{#if p.embedded}<span class="badge"
											><ImageIcon size={11} strokeWidth={2.4} /></span
										>{/if}
								</span>
								{#if p.file}<FileImage size={22} strokeWidth={1.6} />{/if}
							</span>
							<span class="placement-title">{p.label}</span>
							<span class="placement-desc">{placementText(p.id)}</span>
						</label>
					{/each}
				</div>

				{#if setup.art.placement !== 'embedded'}
					<div class="art-name" transition:slide={{ duration: 260, easing: cubicOut }}>
						<label class="name-field" class:bad={artNameBad}>
							<span class="name-label">File name</span>
							<span class="name-input">
								<input
									bind:value={setup.art.file_name}
									placeholder="cover"
									spellcheck="false"
									autocomplete="off"
								/>
								<span class="ext">.jpg</span>
							</span>
						</label>
						<div class="name-suggestions" role="group" aria-label="Suggested file names">
							{#each setup.artFileNames as n (n)}
								<button
									class="suggestion"
									class:on={artName === n}
									onclick={() => (setup.art.file_name = n)}>{n}</button
								>
							{/each}
						</div>
						<p class="name-hint" class:bad={artNameBad}>
							{#if artNameBad}
								A file name can't contain slashes or start with a dot.
							{:else}
								Whatever the image was called in the Inbox, it's saved as <code>{artName}.jpg</code
								>, or <code>.png</code> / <code>.webp</code> to match the image. Other images stay behind.
							{/if}
						</p>
					</div>
				{/if}
			</div>
		</section>

		<SaveBar
			changes={setup.changes}
			saving={setup.saving}
			saveLabel="Save"
			onrevert={() => setup.revert()}
			onsave={() => setup.save()}
		/>
	{/if}
</SetupPane>

<style>
	.rule-text p {
		margin: 2px 0 0;
		color: var(--text-2);
		font-size: 13.5px;
	}

	.rule {
		display: grid;
		gap: 14px;
	}

	.rule-head {
		display: flex;
		align-items: center;
		gap: 12px;
	}

	.rule-icon {
		display: grid;
		place-items: center;
		width: 32px;
		height: 32px;
		border-radius: 10px;
		background: var(--surface-hover);
		color: var(--text-2);
		flex: none;
	}

	.rule-text {
		flex: 1;
		min-width: 0;
	}

	h3 {
		margin: 0;
		font-size: 15px;
	}

	.rule-actions {
		display: flex;
		gap: 8px;
	}

	.art-rule {
		border-top: 1px solid var(--border);
	}
	.placements {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 12px;
	}
	.placement {
		position: relative;
		display: grid;
		align-content: start;
		gap: 4px;
		padding: 14px 16px 16px;
		border-radius: 14px;
		border: 1px solid var(--border);
		background: var(--surface);
		cursor: pointer;
		transition:
			border-color 200ms,
			background 200ms,
			box-shadow 260ms,
			transform 200ms var(--ease-out);
	}
	.placement:hover:not(.on) {
		border-color: var(--border-strong);
		transform: translateY(-1px);
	}
	.placement.on {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 12%, transparent);
	}
	/* The radio stays native (keyboard and screen readers) but sits in the corner. */
	.placement input {
		position: absolute;
		top: 14px;
		right: 14px;
		appearance: none;
		margin: 0;
		width: 18px;
		height: 18px;
		border-radius: 999px;
		border: 1.5px solid var(--border-strong);
		background: var(--bg);
		cursor: pointer;
		transition:
			border-color 160ms,
			box-shadow 220ms var(--ease-spring);
	}
	.placement input:checked {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 4px var(--bg-raised);
		background: var(--accent);
	}
	.placement input:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.glyph {
		display: flex;
		align-items: center;
		gap: 4px;
		height: 30px;
		margin-bottom: 6px;
		color: var(--text-3);
		transition: color 200ms;
	}
	.on .glyph {
		color: var(--accent);
	}
	.audio {
		position: relative;
		display: grid;
	}
	.badge {
		position: absolute;
		right: -5px;
		bottom: -3px;
		display: grid;
		place-items: center;
		width: 16px;
		height: 16px;
		border-radius: 5px;
		background: var(--accent);
		color: var(--bg-raised);
		transform: scale(0.9);
		transition: transform 260ms var(--ease-spring);
	}
	.on .badge {
		transform: scale(1);
	}
	.placement-title {
		font-weight: 620;
		font-size: 14.5px;
	}
	.placement-desc {
		font-size: 13px;
		color: var(--text-2);
	}

	.art-name {
		display: grid;
		gap: 10px;
	}
	.name-field {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.name-label {
		font-size: 13.5px;
		font-weight: 560;
		color: var(--text-2);
	}
	.name-input {
		display: flex;
		align-items: center;
		height: 40px;
		padding: 0 12px;
		border-radius: 10px;
		border: 1px solid var(--border);
		background: var(--surface);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 14px;
		transition:
			border-color 160ms,
			box-shadow 200ms;
	}
	.name-input:focus-within {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.bad .name-input {
		border-color: color-mix(in oklab, var(--danger) 60%, transparent);
	}
	.name-input input {
		width: 14ch;
		border: 0;
		outline: 0;
		background: none;
		color: var(--text);
		font: inherit;
		text-align: right;
	}
	.ext {
		color: var(--text-3);
	}
	.name-suggestions {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.suggestion {
		padding: 4px 10px;
		border-radius: 8px;
		border: 1px solid var(--border);
		background: var(--surface);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 12px;
		color: var(--text-2);
		transition:
			border-color 160ms,
			color 160ms,
			background 160ms;
	}
	.suggestion:hover {
		border-color: var(--border-strong);
		color: var(--text);
	}
	.suggestion.on {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 10%, var(--surface));
		color: var(--text);
	}
	.name-hint {
		margin: 0;
		font-size: 13px;
		color: var(--text-3);
	}
	.name-hint.bad {
		color: var(--danger);
	}
	.name-hint code {
		font-size: 12.5px;
		color: var(--text-2);
	}

	.impact {
		display: grid;
		grid-template-columns: 1fr auto;
		gap: 8px 12px;
		align-items: baseline;
		padding: 14px 16px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--border);
	}

	.impact-text {
		font-size: 14px;
		color: var(--text-2);
	}

	.impact-text b {
		font-size: 20px;
		color: var(--text);
		margin-right: 2px;
	}

	.muted {
		color: var(--text-3);
	}

	.required {
		font-size: 12.5px;
		color: var(--text-3);
	}

	.meter {
		grid-column: 1 / -1;
		height: 6px;
		border-radius: 999px;
		background: var(--border);
		overflow: hidden;
	}

	.meter span {
		display: block;
		height: 100%;
		border-radius: inherit;
		background: linear-gradient(
			90deg,
			color-mix(in oklab, var(--accent) 70%, transparent),
			var(--accent)
		);
		transform-origin: left;
		transition: transform 600ms var(--ease-out);
	}

	.impact-note {
		grid-column: 1 / -1;
		font-size: 12.5px;
		color: var(--text-3);
	}

	.group {
		border-radius: 14px;
		border: 1px solid var(--border);
		transition:
			border-color 200ms,
			background 200ms;
	}

	.group.open {
		background: color-mix(in oklab, var(--surface) 50%, transparent);
	}

	.group-head {
		display: flex;
		align-items: center;
		padding-right: 14px;
	}

	.group-toggle {
		flex: 1;
		display: flex;
		align-items: center;
		gap: 8px;
		height: 46px;
		padding: 0 14px;
		text-align: left;
		font-weight: 600;
		font-size: 14px;
	}

	.chevron {
		display: grid;
		color: var(--text-3);
		transition: transform 280ms var(--ease-out);
	}

	.open .chevron {
		transform: rotate(90deg);
	}

	.group-count {
		margin-left: 4px;
		padding: 1px 8px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-3);
		background: var(--surface-hover);
		transition:
			background 200ms,
			color 200ms;
	}

	.group-count.active {
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 18%, transparent);
	}

	.all {
		display: grid;
		place-items: center;
		width: 32px;
		height: 32px;
		cursor: pointer;
	}

	.kinds {
		list-style: none;
		margin: 0;
		padding: 0 8px 10px;
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 2px 12px;
	}

	.kind {
		display: grid;
		grid-template-columns: 18px 1fr auto;
		align-items: center;
		gap: 12px;
		height: 40px;
		padding: 0 10px;
		border-radius: 10px;
		cursor: pointer;
		transition: background 140ms;
	}

	.kind:hover {
		background: var(--surface-hover);
	}

	.kind-label {
		font-size: 14px;
		color: var(--text-2);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		transition: color 160ms;
	}

	.kind.on .kind-label {
		color: var(--text);
		font-weight: 540;
	}

	.share {
		display: flex;
		align-items: center;
		gap: 8px;
	}

	.share-bar {
		width: 44px;
		height: 4px;
		border-radius: 999px;
		background: var(--border);
		overflow: hidden;
	}

	.share-bar i {
		display: block;
		height: 100%;
		border-radius: inherit;
		background: var(--text-3);
		transition:
			width 600ms var(--ease-out),
			background 200ms;
	}

	.kind.on .share-bar i {
		background: var(--accent);
	}

	.share-pct {
		width: 36px;
		text-align: right;
		font-size: 12px;
		color: var(--text-3);
	}

	.share-pct.none {
		opacity: 0.6;
	}

	/* Checkboxes: native inputs, restyled, with a check that draws in. */
	input[type='checkbox'] {
		appearance: none;
		margin: 0;
		width: 18px;
		height: 18px;
		border-radius: 6px;
		border: 1.5px solid var(--border-strong);
		background: var(--bg);
		display: grid;
		place-items: center;
		cursor: pointer;
		transition:
			background 160ms,
			border-color 160ms,
			transform 160ms var(--ease-spring);
	}

	input[type='checkbox']::before {
		content: '';
		width: 10px;
		height: 6px;
		margin-top: -2px;
		border: 2px solid var(--accent-ink);
		border-top: 0;
		border-right: 0;
		transform: rotate(-45deg) scale(0);
		transition: transform 220ms var(--ease-spring);
	}

	input[type='checkbox']:checked,
	input[type='checkbox']:indeterminate {
		background: var(--accent);
		border-color: var(--accent);
	}

	input[type='checkbox']:checked::before {
		transform: rotate(-45deg) scale(1);
	}

	input[type='checkbox']:indeterminate::before {
		width: 8px;
		height: 0;
		margin-top: 0;
		border-left: 0;
		border-bottom: 2px solid var(--accent-ink);
		transform: none;
	}

	input[type='checkbox']:active {
		transform: scale(0.88);
	}

	.no-match {
		margin: 4px 0;
		color: var(--text-3);
	}

	@media (max-width: 760px) {
		.kinds {
			grid-template-columns: 1fr;
		}
		.rule-head {
			flex-wrap: wrap;
		}
		.rule-actions {
			width: 100%;
		}
		.placements {
			grid-template-columns: 1fr;
		}
	}
</style>
