// Typed client for the coversyncd HTTP API.

import { resolve } from '$app/paths';

/** Path prefix the app is served under ('' at the root, e.g. '/coversync' behind a reverse proxy). */
export const BASE = resolve('/').replace(/\/$/, '');

export interface Album {
	id: string;
	title: string;
	artist: string;
	year: number | null;
	genre: string | null;
	track_count: number;
	duration_ms: number;
	color: string | null;
	has_cover: boolean;
	added_at: number;
	format: string;
	collection: Collection;
	/** Album-wide values for sort tabs beyond the built-in fields. */
	sort_values?: Record<string, string>;
	/** Every value of each sidebar filter tag on any track, several in one tag split apart. */
	filter_values?: Record<string, string[]>;
	/** Present when tracks lack tags the library rules or folder structure need. */
	attention?: { missing: string[]; tracks: number };
}

export type Collection = 'inbox' | 'library';

export interface Track {
	id: number;
	album_id: string;
	title: string;
	artist: string;
	album: string;
	album_artist: string | null;
	track_no: number | null;
	track_total: number | null;
	disc_no: number | null;
	disc_total: number | null;
	year: number | null;
	genre: string | null;
	duration_ms: number;
	sample_rate: number | null;
	bit_depth: number | null;
	channels: number | null;
	bitrate: number | null;
	format: string;
	rel_path: string;
}

export interface AlbumDetail extends Album {
	dir: string;
	tracks: Track[];
}

export interface ScanProgress {
	running: boolean;
	phase: string;
	files_seen: number;
	files_read: number;
	albums: number;
	covers_done: number;
	covers_total: number;
	errors: number;
	finished_at: number | null;
}

export interface Status {
	version: string;
	allow_writes: boolean;
	scan: ScanProgress;
}

export type TagField =
	'title' | 'artist' | 'album' | 'album_artist' | 'track_no' | 'disc_no' | 'year' | 'genre';

export type TagEdit = Partial<Record<TagField, string>>;

export interface TagWrite {
	album: TagEdit;
	tracks: Record<number, TagEdit>;
}

export interface TagWriteResult {
	album_id: string;
	files_written: number;
	album: AlbumDetail | null;
}

export interface TagGroup {
	id: string;
	label: string;
}

/** A tag kind library rules can refer to. */
export interface TagKind {
	id: string;
	label: string;
	group: string;
}

/** Where imported albums keep their art. */
export type ArtPlacement = 'file' | 'embedded' | 'both';

export interface ArtRules {
	required: boolean;
	placement: ArtPlacement;
	/** Cover file name without extension ("cover" → cover.jpg). */
	file_name: string;
}

/** Stands for album art in attention flags' `missing`. */
export const ART = 'album_art';

export interface LibrarySetup {
	rules: {
		required_tags: string[];
		art: ArtRules;
		/** Joins several values of one tag in MusicBrainz suggestions, e.g. ";". */
		separator: string;
	};
	folder: string | null;
	structure: string;
}

/** A configured folder as coversyncd sees it on disk. */
export interface FolderStatus {
	path: string;
	exists: boolean;
	dir: boolean;
	writable: boolean;
	/** A missing folder can be created (on first import). */
	creatable: boolean;
}

export interface SetupResponse {
	setup: LibrarySetup;
	folder: FolderStatus | null;
	groups: TagGroup[];
	kinds: TagKind[];
	essentials: string[];
	/** Kinds the import editor asks for once per album. */
	album_kinds: string[];
	not_editable: string[];
	suggestions: string[];
	/** Folder structure variables; `group` is the tag group ('core' includes {year}). */
	variables: { id: string; label: string; group: string }[];
	/** Cover file names to suggest. */
	art_file_names: string[];
}

export interface StructurePreview {
	error: string | null;
	requires: string[];
	examples: { from: string; to: string | null; missing: string[] }[];
}

export interface ImportTrack {
	id: number;
	title: string;
	file_name: string;
	values: Record<string, string>;
	missing: string[];
	dest: string | null;
	/** Empty tags autofill sets on this track, by kind. */
	autofilled: Record<string, string>;
}

export interface ImportPlan {
	album_id: string;
	mode: IngestMode;
	library_folder: string | null;
	kinds: string[];
	failing: string[];
	/** Album-wide kinds the folder structure uses that differ between tracks. */
	disagree: string[];
	tracks: ImportTrack[];
	problems: string[];
	/** Kinds autofill sets on at least one track. */
	autofilled: string[];
	art: ArtPlan;
	/** Smart suggestions are on: the import dialog looks the album up. */
	suggest: boolean;
}

/** What happens to the album art on import. */
export interface ArtPlan {
	required: boolean;
	placement: ArtPlacement;
	/** Where the album's art comes from, e.g. "folder.jpg"; null if it has none. */
	source: string | null;
	/** The art's size in pixels. */
	width: number | null;
	height: number | null;
	/** The cover file written beside the tracks, e.g. "cover.jpg". */
	file: string | null;
	/** The rule's cover file name, without extension. */
	file_name: string;
	/** Tracks without embedded art that get the album's (every track for a chosen image). */
	embed: number;
	/** Required, and the album has none: the import needs an image. */
	missing: boolean;
}

export interface ImportResult {
	album_id: string;
	files: number;
	dest_dir: string;
	mode: IngestMode;
	/** What happened to the album art, e.g. "cover.jpg". */
	art: string | null;
}

/** Tag values typed in for an import: track id -> kind id -> value. */
export type ImportEdits = Record<number, Record<string, string>>;

export interface TagCoverage {
	tracks: number;
	albums: number;
	/** One entry per tag kind, in catalog order. */
	kinds: { id: string; tracks: number; albums: number }[];
	/** Per album: indices into `kinds` that every track of the album has. */
	album_kinds: Record<string, number[]>;
	/** Albums with no cover art at all. */
	without_art: number;
}

export type IngestMode = 'copy' | 'move';

/** Assumptions import may make to fill empty tags (see Inbox setup). */
export interface Autofill {
	album_artist_from_artist: boolean;
	various_artists: boolean;
	artist_from_album_artist: boolean;
	album_tags_from_tracks: boolean;
	single_track: boolean;
	single_disc: boolean;
	disc_from_folder: boolean;
	totals: boolean;
	track_from_file_name: boolean;
	title_from_file_name: boolean;
	album_from_folder: boolean;
	year_from_folder: boolean;
}

export interface InboxSetup {
	ingest: IngestMode;
	folder: string | null;
	autofill: Autofill;
	suggestions: {
		enabled: boolean;
		/** Romanized or English tags for releases that don't sort A–Z. */
		prefer_english: boolean;
		/** Romanized Chinese, Japanese and Korean names of people, family name first. */
		family_name_first: boolean;
	};
}

export interface HiddenStats {
	files: number;
	albums: number;
}

export interface InboxSetupResponse {
	setup: InboxSetup;
	folder: FolderStatus | null;
	hidden: HiddenStats;
	suggest: {
		/** The saved AcoustID key's last characters; the key itself stays on the server. */
		key_hint: string | null;
		/** The installed fpcalc's version, if any. */
		fpcalc: string | null;
	};
}

/** A release an album might be, from AcoustID or a MusicBrainz search. */
export interface Candidate {
	id: string;
	title: string;
	artist: string | null;
	date: string | null;
	country: string | null;
	format: string | null;
	track_count: number | null;
	/** The album's tracks AcoustID found on it. */
	matched: number;
}

export interface Suggestion {
	kind: string;
	/** What the import would write without it; null if the tag is empty. */
	current: string | null;
	value: string;
	/** When `value` is romanized: MusicBrainz's value in the original script. */
	original?: string;
	/** The current value is `original`, so the suggestion only romanizes it. */
	romanizes?: boolean;
}

export interface Identification {
	method: 'acoustid' | 'tags' | 'chosen';
	release: {
		id: string;
		title: string;
		artist: string;
		date: string | null;
		country: string | null;
		format: string | null;
		track_count: number;
		disambiguation: string | null;
		/** The Cover Art Archive has a front cover for the release. */
		front_art: boolean;
		/** Whose front cover stands in when the release has none. */
		release_group: string | null;
	} | null;
	candidates: Candidate[];
	fingerprinted: number;
	recognized: number;
	tracks: {
		id: number;
		/** How it was paired with the release's track; null if it wasn't. */
		via: 'acoustid' | 'position' | 'title' | null;
		position: string | null;
		suggestions: Suggestion[];
	}[];
	notes: string[];
	/** Present when the release's titles or names don't sort A–Z. */
	romanization: {
		/** Suggestions are romanized, rather than in the original script. */
		applied: boolean;
		/** People's names from Chinese, Japanese or Korean put the family name first. */
		family_first: boolean;
		/** The release Latin track titles came from, if any. */
		tracklist: { id: string; title: string; translated: boolean } | null;
		/** Album, disc and track titles romanized. */
		titles: number;
		/** Artist and label names romanized. */
		names: number;
		/** Titles and names MusicBrainz has nothing Latin for. */
		left: number;
	} | null;
}

/** MusicBrainz's front cover for a release, downloaded by coversyncd. */
export interface MbArt {
	width: number;
	height: number;
	extension: 'jpg' | 'png' | 'webp';
	/** The release's own, or its release group's when it has none. */
	from: 'release' | 'release_group';
}

export type IdentifyStage =
	'reading' | 'fingerprinting' | 'acoustid' | 'searching' | 'release' | 'romanizing';

export type IdentifyState =
	| { state: 'running'; stage: IdentifyStage; done: number; total: number }
	| { state: 'done'; result: Identification }
	| { state: 'failed'; error: string };

export interface AppearanceSetup {
	/** The top bar's sort menu, in order: tag kind ids or 'added'. */
	sort_tabs: string[];
	/** Tag kinds the sidebar filters by, in its dropdown's order. */
	filter_tags: string[];
	/** The narrowest album grid covers may be, in px; rows stretch them to fit. */
	cover_size: number;
}

export interface CoverSizes {
	min: number;
	max: number;
	default: number;
}

export interface Sortable {
	id: string;
	label: string;
	group: string;
}

export interface AppearanceResponse {
	setup: AppearanceSetup;
	defaults: string[];
	filter_defaults: string[];
	sortable: Sortable[];
	/** Tags the sidebar can filter by, a subset of `sortable`. */
	filterable: Sortable[];
	groups: { id: string; label: string }[];
	cover_sizes: CoverSizes;
}

export class ApiError extends Error {
	constructor(
		public status: number,
		message: string
	) {
		super(message);
	}
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
	const res = await fetch(BASE + path, init);
	if (!res.ok) {
		const body = await res.json().catch(() => ({}));
		throw new ApiError(res.status, body.error ?? res.statusText);
	}
	return res.status === 202 ? (undefined as T) : res.json();
}

export const api = {
	status: () => request<Status>('/api/status'),
	albums: (collection: Collection = 'inbox') =>
		request<Album[]>(`/api/albums?collection=${collection}`),
	album: (id: string) => request<AlbumDetail>(`/api/albums/${id}`),
	rescan: () => request<void>('/api/rescan', { method: 'POST' }),
	setup: () => request<SetupResponse>('/api/library/setup'),
	saveSetup: (setup: LibrarySetup) =>
		request<SetupResponse>('/api/library/setup', {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(setup)
		}),
	coverage: () => request<TagCoverage>('/api/inbox/coverage'),
	inboxSetup: () => request<InboxSetupResponse>('/api/inbox/setup'),
	/** `acoustidKey`: a new key, '' to remove it, undefined to keep it. */
	saveInboxSetup: (setup: InboxSetup, acoustidKey?: string) =>
		request<InboxSetupResponse>('/api/inbox/setup', {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ ...setup, acoustid_key: acoustidKey })
		}),
	/**
	 * Start looking the album up on MusicBrainz (or get the lookup under way).
	 * `romanize` overrides Inbox setup's Prefer English for this album.
	 */
	identify: (album_id: string, release_id?: string, refresh = false, romanize?: boolean) =>
		request<IdentifyState>('/api/identify', {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ album_id, release_id, refresh, romanize })
		}),
	identifyStatus: (album_id: string) =>
		request<IdentifyState>(`/api/identify/${encodeURIComponent(album_id)}`),
	/** Download MusicBrainz's front cover for a release; null when there is none. */
	musicbrainzArt: (release_id: string, group: string | null, front: boolean) =>
		request<MbArt | null>(
			`/api/musicbrainz/art/${release_id}?front=${front}` + (group ? `&group=${group}` : '')
		),
	preview: (structure: string) =>
		request<StructurePreview>(`/api/library/preview?structure=${encodeURIComponent(structure)}`),
	checkFolder: (path: string) =>
		request<FolderStatus | null>(`/api/fs/check?path=${encodeURIComponent(path)}`),
	importPlan: (album_id: string, edits: ImportEdits = {}) =>
		request<ImportPlan>('/api/import/plan', {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ album_id, edits })
		}),
	/**
	 * `art`: album art chosen for the import, base64-encoded; or
	 * `musicbrainzArt`, the release whose MusicBrainz cover to use.
	 */
	importAlbum: (album_id: string, edits: ImportEdits = {}, art?: string, musicbrainzArt?: string) =>
		request<ImportResult>('/api/import', {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ album_id, edits, art, musicbrainz_art: musicbrainzArt })
		}),
	appearance: () => request<AppearanceResponse>('/api/appearance'),
	saveAppearance: (setup: AppearanceSetup) =>
		request<AppearanceResponse>('/api/appearance', {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(setup)
		}),
	unhideAll: () =>
		request<{ unhidden: number; hidden: HiddenStats }>('/api/inbox/unhide', { method: 'POST' }),
	writeTags: (id: string, body: TagWrite) =>
		request<TagWriteResult>(`/api/albums/${id}/tags`, {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(body)
		})
};

export const coverUrl = (id: string, size = 512) => `${BASE}/api/albums/${id}/cover?size=${size}`;
/** The album's art at full size. */
export const fullCoverUrl = (id: string) => `${BASE}/api/albums/${id}/cover/full`;
/** MusicBrainz's cover, once `api.musicbrainzArt` has downloaded it. */
export const musicbrainzArtUrl = (release_id: string) =>
	`${BASE}/api/musicbrainz/art/${release_id}/image`;
export const streamUrl = (trackId: number) => `${BASE}/api/tracks/${trackId}/stream`;
