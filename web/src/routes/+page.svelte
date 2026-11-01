<script lang="ts">
	import { onMount, tick, untrack } from 'svelte';
	import { cubicOut } from 'svelte/easing';
	import { fade, fly, slide } from 'svelte/transition';
	import { goto, pushState, replaceState } from '$app/navigation';
	import { page } from '$app/state';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import FolderCog from '@lucide/svelte/icons/folder-cog';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import Inbox from '@lucide/svelte/icons/inbox';
	import LibraryBig from '@lucide/svelte/icons/library-big';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import PanelTopOpen from '@lucide/svelte/icons/panel-top-open';
	import Play from '@lucide/svelte/icons/play';
	import FilterX from '@lucide/svelte/icons/funnel-x';
	import {
		api,
		ApiError,
		type Album,
		type AlbumDetail as Detail,
		type ImportEdits,
		type ImportPlan,
		type Status,
		type TagWriteResult
	} from '#lib/api.ts';
	import {
		collection,
		isSetup,
		migrateSort,
		parseView,
		readPref,
		writePref,
		VIEWS,
		type Sort,
		type View
	} from '#lib/browse.ts';
	import { setup } from '#lib/setup.svelte.ts';
	import { appearance } from '#lib/appearance.svelte.ts';
	import { inboxSetup } from '#lib/inbox-setup.svelte.ts';
	import { syncSetup } from '#lib/sync-setup.svelte.ts';
	import { matcher, picked, type Filters } from '#lib/filters.ts';
	import { count, fold } from '#lib/format.ts';
	import { GRID_MAX_WIDTH, gridLayout } from '#lib/grid.ts';
	import { player } from '#lib/player.svelte.ts';
	import { dismiss, toast, type ToastAction } from '#lib/toast.svelte.ts';
	import TopBar from '#lib/components/TopBar.svelte';
	import SortMenu from '#lib/components/SortMenu.svelte';
	import Sidebar from '#lib/components/Sidebar.svelte';
	import LibrarySetup from '#lib/components/LibrarySetup.svelte';
	import InboxSetup from '#lib/components/InboxSetup.svelte';
	import AppearanceSetup from '#lib/components/AppearanceSetup.svelte';
	import SyncSetup from '#lib/components/SyncSetup.svelte';
	import AlbumGrid from '#lib/components/AlbumGrid.svelte';
	import AlbumDetail from '#lib/components/AlbumDetail.svelte';
	import PlayerBar from '#lib/components/PlayerBar.svelte';
	import Toasts from '#lib/components/Toasts.svelte';
	import ContextMenu, { type MenuEntry } from '#lib/components/ContextMenu.svelte';
	import ImportDialog from '#lib/components/ImportDialog.svelte';

	const SORT_KEY = 'coversync.sort';
	const SIDEBAR_KEY = 'coversync.sidebar';
	const NARROW = '(max-width: 900px)';

	/** Scanned on-disk files: the unmanaged inbox. */
	let albums = $state<Album[]>([]);
	/** Albums coversync manages, in the Library folder. */
	let library = $state<Album[]>([]);
	let status = $state<Status | null>(null);
	let loaded = $state(false);
	let query = $state('');
	let sort = $state<Sort>(
		migrateSort(readPref(SORT_KEY, (v): v is string => typeof v === 'string', 'album_artist'))
	);
	/** The sort in use: the chosen one, or the menu's first if it was removed in Appearance. */
	let activeSort = $derived(
		appearance.sorts.saved.includes(sort) ? sort : appearance.sorts.saved[0]
	);
	let search = $state<HTMLInputElement>();
	let detail = $state<Detail | null>(null);

	let view = $derived(parseView(page.url.searchParams.get('view')));

	// On narrow screens the sidebar is a drawer that starts closed; whether
	// it's open on wider ones is remembered.
	let narrow = $state(matchMedia(NARROW).matches);
	let sidebarClosed = $state(
		readPref(SIDEBAR_KEY, (v): v is string => v === 'collapsed' || v === 'expanded', 'expanded') ===
			'collapsed'
	);
	let drawerOpen = $state(false);
	let collapsed = $derived(narrow ? !drawerOpen : sidebarClosed);

	function toggleSidebar() {
		if (narrow) drawerOpen = !drawerOpen;
		else {
			sidebarClosed = !sidebarClosed;
			writePref(SIDEBAR_KEY, sidebarClosed ? 'collapsed' : 'expanded');
		}
	}

	$effect(() => writePref(SORT_KEY, sort));

	/** The album grid last shown, or for a setup screen opened directly, its own collection. */
	let lastGrid = $state<'inbox' | 'library'>(
		collection(parseView(page.url.searchParams.get('view'))) ?? 'inbox'
	);
	$effect(() => {
		if (view === 'inbox' || view === 'library') lastGrid = view;
	});
	/**
	 * The album grid shown, or the one behind a setup screen: where its back
	 * button returns to, and what the sidebar's filters list.
	 */
	let browsing = $derived(view === 'inbox' || view === 'library' ? view : lastGrid);
	let setupBack = $derived({
		label: browsing === 'library' ? 'Back to Library' : 'Back to Inbox',
		run: () => setView(browsing)
	});
	/** Views slide in from the side of the tab they come after. */
	let direction = $state(1);
	async function setView(next: View) {
		drawerOpen = false;
		if (next === view) return;
		direction = VIEWS.indexOf(next) > VIEWS.indexOf(view) ? 1 : -1;
		query = '';
		const url = new URL(page.url.href);
		if (next === 'inbox') url.searchParams.delete('view');
		else url.searchParams.set('view', next);
		// Same page, so nothing reloads; keep focus on the tab and return to the top.
		await goto(url, { replace: true, reset: false });
		scrollTo({ top: 0 });
	}
	let source = $derived(browsing === 'library' ? library : albums);

	const collator = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });
	const cmp = (a: string, b: string) => collator.compare(a, b);
	/** Dates read newest first; everything else A to Z. */
	const NEWEST_FIRST = new Set([
		'added',
		'date',
		'release_date',
		'original_date',
		'encoding_time',
		'tagging_time'
	]);

	/** An album's value for a sort: built-in fields, else the daemon's sort values. */
	function sortValue(a: Album, key: Sort): string | number | null {
		switch (key) {
			case 'added':
				return a.added_at;
			case 'album_artist':
				return a.artist;
			case 'album':
				return a.title;
			case 'date':
				return a.year;
			case 'genre':
				return a.genre;
			default:
				return a.sort_values?.[key] ?? null;
		}
	}

	/** Albums without a value go last; ties fall back to artist, year, title. */
	function comparator(key: Sort) {
		const sign = NEWEST_FIRST.has(key) ? -1 : 1;
		return (a: Album, b: Album) => {
			const x = sortValue(a, key);
			const y = sortValue(b, key);
			const xMissing = x === null || x === '';
			const yMissing = y === null || y === '';
			if (xMissing !== yMissing) return xMissing ? 1 : -1;
			if (!xMissing) {
				const c =
					typeof x === 'number' && typeof y === 'number' ? x - y : cmp(String(x), String(y));
				if (c) return sign * c;
			}
			return cmp(a.artist, b.artist) || (a.year ?? 0) - (b.year ?? 0) || cmp(a.title, b.title);
		};
	}

	let byId = $derived(new Map([...albums, ...library].map((a) => [a.id, a])));
	let haystack = $derived(
		new Map(
			[...albums, ...library].map((a) => [
				a.id,
				fold(`${a.title} ${a.artist} ${a.genre ?? ''} ${a.year ?? ''}`)
			])
		)
	);
	// Each collection is sorted once per sort order, so switching back and
	// forth is a lookup; searching then filters the sorted list.
	const sortCache = new WeakMap<Album[], Map<Sort, Album[]>>();
	function sorted(list: Album[], by: Sort) {
		let orders = sortCache.get(list);
		if (!orders) sortCache.set(list, (orders = new Map()));
		let out = orders.get(by);
		if (!out) orders.set(by, (out = list.slice().sort(comparator(by))));
		return out;
	}
	/** "Needs attention" filters rather than sorts: flagged albums, A to Z by artist. */
	let attentionView = $derived(activeSort === 'attention');
	let flagged = $derived(source.filter((a) => a.attention));
	/** The sort to fall back to from the attention view; none if it's the only one. */
	let plainSort = $derived(appearance.sorts.saved.find((s) => s !== 'attention'));
	function showAll() {
		if (plainSort) sort = plainSort;
	}
	/** Albums the sort and search leave, in order; the sidebar's tag filters narrow them. */
	let searched = $derived.by(() => {
		const list = attentionView
			? sorted(source, 'album_artist').filter((a) => a.attention)
			: sorted(source, activeSort);
		// Setup screens use the search field for their own lists.
		const terms = isSetup(view) ? [] : fold(query).split(/\s+/).filter(Boolean);
		return terms.length
			? list.filter((a) => terms.every((t) => haystack.get(a.id)!.includes(t)))
			: list;
	});
	/** Tag values picked in the sidebar, kept per collection as their tags differ. */
	let filters = $state<Record<'inbox' | 'library', Filters>>({ inbox: {}, library: {} });
	let filterKinds = $derived(appearance.filters.saved);
	let filtered = $derived(picked(filters[browsing], filterKinds));
	let visible = $derived(
		filtered ? searched.filter(matcher(filters[browsing], filterKinds)) : searched
	);

	/** The album grid's width, for the Appearance screen's cover size preview. */
	let mainBox = $state<DOMRectReadOnly>();
	let gridWidth = $derived(Math.min(GRID_MAX_WIDTH, mainBox?.width ?? 0));
	/** The skeleton grid's cards match the real ones'. */
	let ghostWidth = $derived(gridLayout(gridWidth, appearance.savedCoverSize).minWidth);
	/** The top of the grid last browsed, for previewing cover sizes with real covers. */
	let previewAlbums = $derived.by(() => {
		const list = browsing === 'library' && library.length ? library : albums;
		return sorted(list, activeSort === 'attention' ? 'album_artist' : activeSort).slice(0, 12);
	});

	/** Card badge in the attention view: what's missing, and on how many tracks. */
	function attentionFlag(a: Album) {
		const at = a.attention;
		if (!at) return null;
		const names = at.missing.map((k) => setup.label(k));
		return {
			text: names.length === 1 ? names[0] : `${names.length} missing`,
			title: `Missing ${names.join(', ')} on ${at.tracks} of ${a.track_count} ${a.track_count === 1 ? 'track' : 'tracks'}`
		};
	}

	let openId = $derived(page.state.album ?? null);
	let openAlbum = $derived(openId ? (byId.get(openId) ?? null) : null);

	// Load the open album's tracks.
	$effect(() => {
		const id = openId;
		if (!id || untrack(() => detail?.id) === id) return;
		let cancelled = false;
		api
			.album(id)
			.then((d) => !cancelled && (detail = d))
			.catch(() => toast('Could not load album', 'error'));
		return () => (cancelled = true);
	});

	$effect(() => {
		document.documentElement.style.overflow = openAlbum || importing ? 'hidden' : '';
	});

	// Room the player takes at the bottom of the window, for bars that float above it.
	$effect(() => {
		document.documentElement.style.setProperty(
			'--dock',
			player.current ? 'calc(var(--player-h) + 12px)' : '0px'
		);
	});

	const reducedMotion = () => matchMedia('(prefers-reduced-motion: reduce)').matches;

	/** Run a DOM update as a view transition morphing `from` into the other cover. */
	async function morph(from: HTMLElement | null, update: () => Promise<void>) {
		if (!from || !document.startViewTransition || reducedMotion()) return update();
		from.style.viewTransitionName = 'album-cover';
		const vt = document.startViewTransition(async () => {
			from.style.viewTransitionName = '';
			await update();
		});
		await vt.updateCallbackDone.catch(() => {});
	}

	function gridCover(id: string): HTMLElement | null {
		const el = document.querySelector<HTMLElement>(`[data-album="${id}"] .cover`);
		if (!el) return null;
		const r = el.getBoundingClientRect();
		return r.bottom > 0 && r.top < innerHeight ? el : null;
	}

	async function open(album: Album, cover: HTMLElement | null = gridCover(album.id)) {
		if (openId === album.id) return;
		await morph(cover, async () => {
			await pushState('', { album: album.id });
			await tick();
		});
	}

	function back(): Promise<void> {
		return new Promise((resolve) => {
			const done = () => tick().then(() => resolve());
			const timer = setTimeout(done, 400);
			addEventListener('popstate', () => (clearTimeout(timer), done()), { once: true });
			history.back();
		});
	}

	async function close() {
		const id = openId;
		if (!id) return;
		const heroCover = document.querySelector<HTMLElement>('.hero-cover');
		const target = gridCover(id);
		if (!target || !document.startViewTransition || reducedMotion()) return back();
		const vt = document.startViewTransition(async () => {
			await back();
			target.style.viewTransitionName = 'album-cover';
		});
		heroCover?.style.setProperty('view-transition-name', 'album-cover');
		vt.finished.finally(() => (target.style.viewTransitionName = ''));
	}

	async function playAlbum(album: Album) {
		try {
			const d = detail?.id === album.id ? detail : await api.album(album.id);
			player.playAlbum(album, d.tracks, 0);
		} catch {
			toast('Could not load album', 'error');
		}
	}

	async function onsaved(result: TagWriteResult) {
		await loadAlbums();
		if (result.album) detail = result.album;
		if (result.album_id !== openId) await replaceState('', { album: result.album_id });
	}

	let lastAlbums = 0;
	async function loadAlbums() {
		try {
			[albums, library] = await Promise.all([api.albums('inbox'), api.albums('library')]);
			lastAlbums = Date.now();
		} catch {
			toast('Could not reach coversyncd', 'error');
		}
		loaded = true;
	}

	// New rules or folder structure change which albums need attention.
	$effect(() => {
		if (setup.version + inboxSetup.version > 0) untrack(() => loadAlbums());
	});

	/** Bumped whenever the visible Inbox changes (a scan finished, files were unhidden). */
	let dataGeneration = $state(0);

	async function inboxChanged() {
		await loadAlbums();
		dataGeneration++;
	}
	let pollTimer: ReturnType<typeof setTimeout>;
	async function poll() {
		clearTimeout(pollTimer);
		try {
			const wasRunning = status?.scan.running ?? false;
			status = await api.status();
			if (wasRunning && !status.scan.running) dataGeneration++;
			if (status.scan.running ? Date.now() - lastAlbums > 4000 : wasRunning) await loadAlbums();
		} catch {}
		pollTimer = setTimeout(poll, status?.scan.running ? 1500 : 10000);
	}

	// Right-click menu on album cards.
	let menu = $state<{ album: Album; x: number; y: number } | null>(null);
	let menuItems = $derived.by((): MenuEntry[] => {
		const a = menu?.album;
		if (!a) return [];
		const items: MenuEntry[] = [
			{ label: 'Open', icon: PanelTopOpen, run: () => open(a) },
			{ label: 'Play', icon: Play, run: () => playAlbum(a) }
		];
		if (a.collection === 'inbox')
			items.push('separator', {
				label: 'Import to Library',
				icon: FolderInput,
				primary: true,
				run: () => startImport(a)
			});
		return items;
	});

	// Import: check the album against the library rules; import straight away
	// when nothing is missing, otherwise open the editor with the gaps marked.
	/** The import editor: the plan plus any edits made before it opened. */
	let importing = $state<{ album: Album; plan: ImportPlan; edits: ImportEdits } | null>(null);

	/** Point toasts about setup problems at the screen that fixes them. */
	function fixAction(problem: string): ToastAction | undefined {
		if (/library folder|folder structure/i.test(problem))
			return { label: 'Library setup', run: () => setView('library-setup') };
		if (/Inbox folder|Move/i.test(problem))
			return { label: 'Inbox setup', run: () => setView('inbox-setup') };
	}

	/** `edits` come from the album's tag editor, if the import started there. */
	async function startImport(album: Album, edits: ImportEdits = {}) {
		const checking = toast(`Checking “${album.title}”…`, 'progress', 0);
		let plan: ImportPlan;
		try {
			plan = await api.importPlan(album.id, edits);
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Could not check the album', 'error', 6000);
			return;
		} finally {
			dismiss(checking);
		}
		if (plan.failing.length || plan.disagree.length || plan.art.missing || plan.suggest) {
			// The editor needs the tag catalog (labels, album-wide kinds) up front.
			await setup.load();
			importing = { album, plan, edits };
		} else if (plan.problems.length) {
			toast(plan.problems.join(' '), 'error', 8000, fixAction(plan.problems[0]));
		} else {
			await runImport(album, edits, plan.autofilled);
		}
	}

	/** Returns an error message, or null when the album was imported. */
	/** `autofilled`: kinds autofill sets, mentioned in the success toast. */
	async function runImport(
		album: Album,
		edits: ImportEdits,
		autofilled: string[] = [],
		art?: string,
		suggested = 0,
		musicbrainzArt?: string
	): Promise<string | null> {
		const working = toast(`Importing “${album.title}”…`, 'progress', 0);
		try {
			const r = await api.importAlbum(album.id, edits, art, musicbrainzArt);
			importing = null;
			// The album leaves the Inbox; close its sheet first so history stays tidy.
			if (openId === album.id) await close();
			await loadAlbums();
			dataGeneration++;
			const how = r.mode === 'copy' ? 'copied' : 'moved';
			const filled = autofilled.length
				? ` · filled in ${autofilled.map((k) => setup.label(k)).join(', ')}`
				: '';
			const placed = r.art ? ` · ${r.art}` : '';
			const taken = [
				suggested && `${suggested} ${suggested === 1 ? 'tag' : 'tags'}`,
				musicbrainzArt && 'the cover'
			].filter(Boolean);
			const fromMb = taken.length ? ` · ${taken.join(' and ')} from MusicBrainz` : '';
			toast(
				`Imported “${album.title}”: ${r.files} ${r.files === 1 ? 'file' : 'files'} ${how}${filled}${fromMb}${placed}`,
				'success',
				7000,
				{ label: 'Show', run: () => showInLibrary(r.album_id) }
			);
			return null;
		} catch (e) {
			const message = e instanceof ApiError ? e.message : 'The import failed';
			if (!importing) toast(message, 'error', 8000, fixAction(message));
			return message;
		} finally {
			dismiss(working);
		}
	}

	async function showInLibrary(id: string) {
		await setView('library');
		await tick();
		const album = byId.get(id);
		if (album) open(album);
	}

	async function rescan() {
		await api.rescan().catch(() => toast('Rescan failed to start', 'error'));
		setTimeout(poll, 300);
	}

	/** The key went to an element matching `selector` (the target may be the window). */
	function within(e: KeyboardEvent, selector: string) {
		return e.target instanceof Element && !!e.target.closest(selector);
	}
	const typing = (e: KeyboardEvent) => within(e, 'input, textarea, [contenteditable]');
	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape' && openId) {
			e.preventDefault();
			close();
		} else if (e.key === 'Escape' && narrow && drawerOpen) {
			drawerOpen = false;
		} else if (e.key === 'Escape' && isSetup(view) && !typing(e)) {
			setView(browsing);
		} else if (typing(e) || e.ctrlKey || e.metaKey || e.altKey) {
			return;
		} else if (e.key === '/') {
			e.preventDefault();
			search?.focus();
		} else if (e.key === ' ' && !within(e, 'button, [role=slider]')) {
			e.preventDefault();
			player.toggle();
		}
	}

	function clearFilters() {
		filters[browsing] = {};
	}

	onMount(() => {
		appearance.load();
		// Whether the sidebar shows its Sync section.
		syncSetup.load();
		// Tag labels for attention badges.
		setup.load();
		loadAlbums();
		poll();
		const mq = matchMedia(NARROW);
		const onchange = () => {
			narrow = mq.matches;
			drawerOpen = false;
		};
		mq.addEventListener('change', onchange);
		return () => {
			clearTimeout(pollTimer);
			mq.removeEventListener('change', onchange);
		};
	});
</script>

<svelte:window {onkeydown} />

<TopBar
	bind:query
	bind:search
	inert={!!openAlbum}
	{view}
	counts={{ inbox: loaded ? albums.length : null, library: loaded ? library.length : null }}
	scan={status?.scan ?? null}
	shown={visible.length}
	total={source.length}
	sidebarOpen={!collapsed}
	{filtered}
	onview={setView}
	ontogglesidebar={toggleSidebar}
	onrescan={rescan}
>
	{#snippet sorts()}
		<SortMenu
			sorts={appearance.sorts.saved}
			active={activeSort}
			label={(id) => appearance.label(id)}
			counts={{ attention: flagged.length }}
			onsort={(id) => (sort = id)}
			disabled={isSetup(view)}
		/>
	{/snippet}
</TopBar>

<div class="shell" inert={!!openAlbum}>
	<Sidebar
		kinds={filterKinds}
		label={(id) => appearance.label(id)}
		albums={searched}
		bind:filters={filters[browsing]}
		{collapsed}
		{view}
		sync={syncSetup.loaded && syncSetup.savedEnabled}
		dirty={{
			'inbox-setup': inboxSetup.changes > 0,
			'library-setup': setup.changes > 0,
			appearance: appearance.changes > 0,
			'sync-setup': syncSetup.changes > 0
		}}
		onview={setView}
	/>

	<main bind:contentRect={mainBox}>
		{#key view}
			<div class="view" in:fly={{ x: 28 * direction, duration: 420, easing: cubicOut }}>
				{#if attentionView && !isSetup(view) && loaded && appearance.settled && source.length}
					<!-- A filter kept across visits, so it's always on show while it applies. -->
					<div class="attention-note" role="status" transition:slide={{ duration: 220 }}>
						<CircleAlert size={15} strokeWidth={2.2} />
						<span>
							Only albums that need attention: <b class="tabular">{count(flagged.length)}</b> of
							{count(source.length)}
						</span>
						{#if plainSort}
							<button class="link" onclick={showAll}>Show all albums</button>
						{/if}
					</div>
				{/if}
				{#if view === 'library-setup'}
					<LibrarySetup
						{query}
						back={setupBack}
						scanning={status?.scan.running ?? false}
						generation={dataGeneration}
					/>
				{:else if view === 'appearance'}
					<AppearanceSetup
						{query}
						back={setupBack}
						onsaved={loadAlbums}
						{gridWidth}
						albums={previewAlbums}
					/>
				{:else if view === 'inbox-setup'}
					<InboxSetup
						back={setupBack}
						onchanged={inboxChanged}
						onrescan={() => setTimeout(poll, 300)}
					/>
				{:else if view === 'sync-setup'}
					<SyncSetup back={setupBack} />
				{:else if !loaded || !appearance.settled}
					<div class="grid" aria-busy="true" style:--cover-min="{ghostWidth}px">
						{#each { length: 18 } as _, i (i)}
							<div class="ghost" style:--i={i}><span></span><b></b><b></b></div>
						{/each}
					</div>
				{:else if view === 'library' && library.length === 0}
					<div class="empty">
						<div class="fan" aria-hidden="true">
							<span></span><span></span><span><LibraryBig size={30} strokeWidth={1.4} /></span>
						</div>
						<h2>Your library is empty</h2>
						<p>Albums you bring in from the Inbox will be organized and managed here.</p>
						<div class="empty-actions">
							<button class="btn primary" onclick={() => setView('library-setup')}>
								<Settings2 size={16} /> Set up rules
							</button>
							<button class="btn" onclick={() => setView('inbox')}>
								<Inbox size={16} /> Browse the Inbox
							</button>
						</div>
					</div>
				{:else if source.length === 0}
					<div class="empty">
						<span class="empty-icon"><Disc3 size={44} strokeWidth={1.2} /></span>
						{#if status?.scan.running}
							<h2>Scanning for music…</h2>
							<p>Albums appear in the Inbox as their tags are read.</p>
						{:else}
							<h2>The Inbox is empty</h2>
							<p>No audio files were found in the Inbox folder.</p>
							<div class="empty-actions">
								<button class="btn primary" onclick={() => setView('inbox-setup')}>
									<FolderCog size={16} /> Inbox setup
								</button>
								<button class="btn" onclick={rescan}>
									<RefreshCw size={16} /> Rescan
								</button>
							</div>
						{/if}
					</div>
				{:else if visible.length === 0 && attentionView && !query.trim() && !filtered}
					<div class="empty">
						<span class="empty-ok"><CircleCheck size={44} strokeWidth={1.4} /></span>
						<h2>Nothing needs attention</h2>
						<p>Every album here has the tags the library rules and folder structure need.</p>
						{#if plainSort}
							<div class="empty-actions">
								<button class="btn" onclick={showAll}>Show all albums</button>
							</div>
						{/if}
					</div>
				{:else if visible.length === 0 && filtered}
					<div class="empty">
						<h2>
							{attentionView
								? 'No albums needing attention match these filters'
								: 'No albums match these filters'}
						</h2>
						<p>
							{query.trim() ? `Nothing with these tags matches “${query}”.` : 'Try fewer tags.'}
						</p>
						<div class="empty-actions">
							<button class="btn" onclick={clearFilters}>
								<FilterX size={16} /> Clear filters
							</button>
							{#if attentionView && plainSort}
								<button class="btn" onclick={showAll}>Show all albums</button>
							{/if}
						</div>
					</div>
				{:else if visible.length === 0}
					<div class="empty">
						<h2>
							{attentionView
								? `Nothing that needs attention matches “${query}”`
								: `Nothing matches “${query}”`}
						</h2>
						<p>Try an artist, album title, genre or year.</p>
						{#if attentionView && plainSort}
							<div class="empty-actions">
								<button class="btn" onclick={showAll}>Search all albums</button>
							</div>
						{/if}
					</div>
				{:else}
					<AlbumGrid
						albums={visible}
						size={appearance.savedCoverSize}
						onopen={open}
						onplay={playAlbum}
						onmenu={(a, x, y) => (menu = { album: a, x, y })}
						flag={attentionView ? attentionFlag : undefined}
					/>
				{/if}
			</div>
		{/key}
	</main>
</div>

{#if narrow && drawerOpen}
	<button
		class="scrim"
		aria-label="Close sidebar"
		onclick={() => (drawerOpen = false)}
		transition:fade={{ duration: 260 }}
	></button>
{/if}

{#if openAlbum}
	<AlbumDetail
		album={openAlbum}
		detail={detail?.id === openAlbum.id ? detail : null}
		allowWrites={openAlbum.collection === 'library' || (status?.allow_writes ?? false)}
		onclose={close}
		{onsaved}
		onimport={openAlbum.collection === 'inbox'
			? (edits) => startImport(openAlbum!, edits)
			: undefined}
	/>
{/if}

<PlayerBar onopen={(id) => byId.get(id) && open(byId.get(id)!)} />
{#if menu}
	<!-- Keyed so a right-click on another card places a fresh menu. -->
	{#key menu}
		<ContextMenu
			x={menu.x}
			y={menu.y}
			title={menu.album.title}
			items={menuItems}
			onclose={() => (menu = null)}
		/>
	{/key}
{/if}

{#if importing}
	<ImportDialog
		album={importing.album}
		plan={importing.plan}
		baseEdits={importing.edits}
		label={(id) => setup.label(id)}
		albumKinds={setup.albumKinds}
		notEditable={setup.notEditable}
		coreKinds={setup.kinds.filter((k) => k.group === 'core').map((k) => k.id)}
		kindOrder={setup.kinds.map((k) => k.id)}
		onclose={() => (importing = null)}
		onimport={(edits, art, suggested, mbArt) =>
			runImport(importing!.album, edits, importing!.plan.autofilled, art, suggested, mbArt)}
	/>
{/if}

<Toasts />

<style>
	.shell {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
	}
	main {
		/* Its own column even when the sidebar is a drawer, out of the grid's flow;
		   the auto column would shrink it to nothing, as the album grid has no
		   intrinsic width. */
		grid-column: 2;
		min-width: 0;
		padding: 28px 28px calc(var(--player-h) + 56px);
	}
	/* The attention view is a filter: say so above the grid, with the way out. */
	.attention-note {
		display: flex;
		align-items: center;
		gap: 10px;
		max-width: 1800px;
		margin: 0 auto 22px;
		padding: 8px 8px 8px 14px;
		border-radius: 12px;
		font-size: 13.5px;
		color: var(--text-2);
		background: color-mix(in oklab, hsl(40 90% 60%) 9%, transparent);
		border: 1px solid color-mix(in oklab, hsl(40 90% 60%) 28%, transparent);
	}
	.attention-note :global(svg) {
		flex: none;
		color: hsl(40 85% 58%);
	}
	.attention-note span {
		min-width: 0;
	}
	.attention-note b {
		color: var(--text);
		font-weight: 600;
	}
	.attention-note .link {
		margin-left: auto;
		padding: 4px 10px;
		border-radius: 999px;
		font-size: 13px;
		font-weight: 600;
		color: var(--accent);
		white-space: nowrap;
		transition: background 140ms;
	}
	.attention-note .link:hover {
		background: color-mix(in oklab, var(--accent) 14%, transparent);
	}
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 25;
		background: rgb(0 0 0 / 0.45);
		backdrop-filter: blur(2px);
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(var(--cover-min), 1fr));
		gap: 30px 22px;
		max-width: 1800px;
		margin: 0 auto;
	}
	.ghost {
		display: grid;
		gap: 8px;
		animation: fade-in 400ms both;
		animation-delay: calc(var(--i) * 30ms);
	}
	.ghost span,
	.ghost b {
		border-radius: var(--radius);
		background: linear-gradient(
				90deg,
				var(--surface) 0%,
				var(--surface-hover) 50%,
				var(--surface) 100%
			)
			0 0 / 200% 100%;
		animation: shimmer 1.4s linear infinite;
	}
	.ghost span {
		aspect-ratio: 1;
	}
	.ghost b {
		height: 12px;
		width: 80%;
		border-radius: 6px;
	}
	.ghost b:last-child {
		width: 55%;
	}
	@keyframes shimmer {
		to {
			background-position: -200% 0;
		}
	}
	@keyframes fade-in {
		from {
			opacity: 0;
		}
	}
	.empty {
		display: grid;
		justify-items: center;
		gap: 6px;
		padding: 18vh 24px;
		text-align: center;
		animation: fade-in 500ms both;
	}
	.empty h2 {
		margin: 8px 0 0;
		font-size: 22px;
		letter-spacing: -0.02em;
	}
	.empty p {
		margin: 0;
		color: var(--text-2);
	}
	.empty-actions {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		gap: 10px;
		margin-top: 18px;
	}
	/* Three empty sleeves fanning out: the library waiting to be filled. */
	.fan {
		position: relative;
		width: 96px;
		height: 96px;
		margin-bottom: 22px;
	}
	.fan span {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		border-radius: 18px;
		border: 1.5px dashed var(--border-strong);
		background: var(--bg-raised);
		color: var(--text-3);
		animation: fan-out 900ms var(--ease-spring) 120ms both;
	}
	.fan span:nth-child(1) {
		--to: translateX(-40px) rotate(-12deg) scale(0.88);
		opacity: 0.8;
	}
	.fan span:nth-child(2) {
		--to: translateX(40px) rotate(12deg) scale(0.88);
		opacity: 0.8;
	}
	.fan span:nth-child(3) {
		--to: translateY(-2px);
		border-style: solid;
		border-color: color-mix(in oklab, var(--accent) 45%, transparent);
		color: var(--accent);
		box-shadow: 0 18px 40px -16px color-mix(in oklab, var(--accent) 50%, transparent);
		animation:
			fan-out 900ms var(--ease-spring) 120ms both,
			float 4s ease-in-out 1.1s infinite alternate;
	}
	@keyframes fan-out {
		from {
			transform: none;
		}
		to {
			transform: var(--to);
		}
	}
	@keyframes float {
		from {
			transform: var(--to);
		}
		to {
			transform: translateY(-8px);
		}
	}
	.empty-ok {
		color: var(--success);
		animation: pop 600ms var(--ease-spring) both;
	}
	@keyframes pop {
		from {
			opacity: 0;
			transform: scale(0.6);
		}
	}
	.empty-icon {
		color: var(--text-3);
		animation: spin 6s linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}
	@media (max-width: 600px) {
		main {
			padding: 18px 16px calc(var(--player-h) + 64px);
		}
		.grid {
			gap: 22px 14px;
		}
		.attention-note {
			margin-bottom: 16px;
		}
	}
</style>
