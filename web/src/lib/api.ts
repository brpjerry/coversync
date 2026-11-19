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
	/** Changes whenever the art is redone: cover URLs carry it, so browsers keep covers until then. */
	cover_version: string | null;
	added_at: number;
	format: string;
	collection: Collection;
	/** Album-wide values for sorts beyond the built-in fields. */
	sort_values?: Record<string, string>;
	/** Every value of each sidebar filter tag on any track, several in one tag split apart. */
	filter_values?: Record<string, string[]>;
	/** Present when tracks lack tags the library rules or folder structure need. */
	attention?: { missing: string[]; tracks: number };
	/** Present when the album's own track and disc numbers say some are missing. */
	incomplete?: Incomplete;
}

/** What an album's numbering says it lacks. */
export interface Incomplete {
	/**
	 * Runs of missing track numbers, in order. `disc` is set when the album has
	 * (or should have) several discs, each numbered from 1.
	 */
	tracks: { disc: number | null; from: number; to: number }[];
	/** Discs missing below the album's last one. */
	discs: number[];
	/** More tracks missing than the gaps hold, by an album-wide total: on no disc in particular. */
	unplaced?: number;
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
	/** Paths a tag save moved this Library track from (album details only). */
	moved_from?: string[];
}

export interface AlbumDetail extends Album {
	dir: string;
	tracks: Track[];
	/** Changes whenever any of the album's tracks is written, or a track joins or leaves it. */
	version: string;
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
	hashes: {
		running: boolean;
		files_total: number;
		files_done: number;
		bytes_hashed: number;
		errors: number;
		skipped: number;
	};
	/** null while no Library folder is set up. */
	library: { id: string; generation: number } | null;
	inbox: { generation: number };
}

export type TagField =
	'title' | 'artist' | 'album' | 'album_artist' | 'track_no' | 'disc_no' | 'year' | 'genre';

export type TagEdit = Partial<Record<TagField, string>>;

export interface TagWrite {
	album?: TagEdit;
	tracks?: Record<number, TagEdit>;
	/** Tag kinds (catalog ids) to set, by track id; '' removes one. */
	kinds?: Record<number, Record<string, string>>;
	/** The album's version the edits were made on: if it has changed since, nothing is saved (see TagConflict). */
	version?: string;
	/**
	 * The album's tracks then (ids and paths, from its details), to find it by if it
	 * has a new id since (a new title gives it one).
	 */
	base_tracks?: Pick<Track, 'id' | 'rel_path'>[];
}

/** Every track's tags, as the tag editor shows them. */
export interface AlbumTags {
	/** The album's version as they were read. */
	version: string;
	tracks: {
		id: number;
		rel_path: string;
		/** Each tag kind the track has, by catalog id; several values joined by the separator. */
		values: Record<string, string>;
		/** Tags no kind names, as [key, value] in the file's own naming: kept, not editable. */
		other: [string, string][];
	}[];
}

export interface TagWriteResult {
	album_id: string;
	files_written: number;
	/** Files moved to where the folder structure puts them (Library albums), its other files among them. */
	files_moved: number;
	/** Saved, but the files couldn't all be moved: why. */
	warning?: string;
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
		/** Phrases like "feat." that join names in tags naming people: the
		 * import and suggestions put the separator there, and filters split at them. */
		split_at: string[];
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
	/** A missing folder can be created (on first import or upload). */
	creatable: boolean;
	/** Tracks in the coversync library this folder already holds, if it holds one. */
	library: number | null;
}

export interface SetupResponse {
	setup: LibrarySetup;
	folder: FolderStatus | null;
	/** Saving opened a library that has its own rules, which replaced the ones sent. */
	adopted: boolean;
	/** Why the library folder can't hold its database, if it can't. */
	library_problem: string | null;
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
	/** What the import sets by itself on this track, by kind: empty tags
	 * autofill fills, and names split at Library setup's phrases. */
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
	/** Kinds naming people the import splits at Library setup's phrases. */
	split: string[];
	art: ArtPlan;
	/** Smart suggestions are on: the import dialog looks the album up. */
	suggest: boolean;
	/** The date added (Unix seconds) the album keeps in the Library, its Inbox date,
	 * with Preserve modification date on; null: the import time. */
	added: number | null;
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
	/** Where uploads go; null means Uploads in the Inbox folder. */
	uploads: string | null;
	autofill: Autofill;
	suggestions: {
		enabled: boolean;
		/** Romanized or English tags for releases that don't sort A–Z. */
		prefer_english: boolean;
		/** Romanized Chinese, Japanese and Korean names of people, family name first. */
		family_name_first: boolean;
		/** Keep AcoustID's and MusicBrainz's answers, so an album is looked up once. */
		save_lookups: boolean;
	};
	/** Preserve modification date: imported albums keep their Inbox date as their date added. */
	keep_added: boolean;
}

export interface HiddenStats {
	files: number;
	albums: number;
}

export interface InboxSetupResponse {
	setup: InboxSetup;
	folder: FolderStatus | null;
	/** The folder uploads go to, as set or by default; null without an Inbox folder. */
	uploads: FolderStatus | null;
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
	method: 'acoustid' | 'barcode' | 'isrc' | 'tags' | 'chosen';
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
		via: 'acoustid' | 'isrc' | 'position' | 'title' | null;
		position: string | null;
		suggestions: Suggestion[];
	}[];
	/**
	 * The release's tracks none of the album's were paired with; a disc with
	 * none paired is in `missing_discs` instead. Both empty when nothing was.
	 */
	missing: { position: string; title: string; length_ms: number | null }[];
	missing_discs: {
		position: number;
		format: string | null;
		title: string | null;
		tracks: number;
	}[];
	notes: string[];
	/**
	 * When the oldest saved lookup used was made (Unix seconds); null when
	 * AcoustID and MusicBrainz were asked just now.
	 */
	saved: number | null;
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

export interface SyncSetup {
	/** Sync is on: the sidebar shows its Sync section. */
	enabled: boolean;
}

export interface SyncSetupResponse {
	setup: SyncSetup;
}

/** What a paired device may do: everything but managing devices, or only syncing. */
export type Scope = 'manage' | 'sync';

/** A paired app or player, with its own token. Times are Unix seconds. */
export interface Device {
	id: number;
	name: string;
	scope: Scope;
	/** What it said it runs on: "android", "linux", … */
	platform: string | null;
	hostname: string | null;
	app_version: string | null;
	created_at: number;
	/** When its current token was issued. */
	paired_at: number;
	last_seen_at: number | null;
	last_ip: string | null;
	revoked_at: number | null;
	/**
	 * Its sync settings, when this login may read them: a password login
	 * reads every device's, an app only its own. Changes to the device leave
	 * them out.
	 */
	sync?: DeviceSync | null;
}

/** How a device lays out its copy of the Library, and its art. */
export interface DeviceSettings {
	/** Its own folder structure; null keeps each file's path in the Library. */
	structure: string | null;
	art: {
		/** Write each album's cover as a file beside its tracks. */
		cover_file: boolean;
		/** Without extension ("cover" → cover.jpg). */
		cover_name: string;
		/**
		 * The widest the cover file may be, in pixels; null for no limit. Art
		 * inside the tracks isn't shrunk yet (SYN-12).
		 */
		max_width: number | null;
	};
	auto: AutoSync;
}

/** When a device syncs by itself. Each kind of device applies what fits it. */
export interface AutoSync {
	enabled: boolean;
	/** How long to wait after the last change, so a run of edits becomes one sync: 0 to 3600. */
	quiet_seconds: number;
	wifi_only: boolean;
	charging_only: boolean;
}

/** Files, and their size in bytes. */
export interface FileCount {
	files: number;
	bytes: number;
}

/** What started a sync: someone asked, a change, a periodic check, the device waking. */
export type SyncReason = 'user' | 'event' | 'timer' | 'wake' | 'other';

/** How a sync ended. `partial`: stopped for space, time or a lost network, with work left. */
export type SyncResult = 'complete' | 'partial' | 'cancelled' | 'failed';

/** A file a device couldn't sync. `error`: no_space, path_too_long, protected, changed, gone, io, … */
export interface SyncFailure {
	path: string;
	hash: string;
	op: string;
	error: string;
	detail: string | null;
	at: number;
}

/** A device's sync as Sync setup shows it (`GET /api/devices/{id}/sync/state`). Times are Unix seconds. */
export interface DeviceSyncState {
	/** A sync under way. */
	running: {
		started_at: number;
		reason: SyncReason;
		/** What it has done so far; `rate` in bytes a second, lately. */
		progress: { files: number; bytes: number; rate: number | null };
		/** The whole sync's size, once it's planned. */
		totals: {
			delete: number;
			move: number;
			rmdir: number;
			mkdir: number;
			put: number;
			put_bytes: number;
			preparing: number;
			short_bytes: number;
		} | null;
		/** Looking through the device's folder before planning. */
		reconciling: boolean;
	} | null;
	last: {
		started_at: number;
		ended_at: number;
		reason: SyncReason;
		report: {
			result: SyncResult;
			message?: string | null;
			files: number;
			bytes: number;
			seconds: number;
			free_bytes: number | null;
		};
	} | null;
	/** Its folder has been looked through once at least. */
	reconciled: boolean;
	/** What the device holds, as far as coversync knows. */
	held: FileCount;
	/** What it still needs: files to copy, move or delete, and the bytes to copy. */
	pending: FileCount;
	/** Files not ready to send yet. */
	preparing: number;
	/** Files whose paths are too long for the device. */
	too_long: number;
	/** Room it would still need, from the free space it last reported. */
	short_bytes: number;
	/** At most 50 of them listed, newest first. */
	failed: { count: number; files: SyncFailure[] };
	/** Files in its folder that coversync didn't put there. */
	other: FileCount;
	/** Those are removed when it syncs. */
	remove_other: boolean;
	check_requested: boolean;
	sync_requested: boolean;
	/** The Library folder isn't available: there's nothing to sync. */
	no_library: boolean;
}

/** What Sync setup can ask of a device's sync (a password login only). */
export type SyncAction = 'sync' | 'check' | 'retry' | 'remove_other' | 'keep_other';

/** A device's sync settings, and their version (0 until first saved). */
export interface DeviceSync {
	version: number;
	settings: DeviceSettings;
}

export interface DevicesResponse {
	devices: Device[];
	/** The device this page is signed in as, through its token. */
	this_device: number | null;
	/** Signed in with the password: only that may add, change or revoke devices. */
	can_manage: boolean;
}

/** A pairing code, just made by Add device. */
export interface NewPairing {
	id: string;
	/** 8 characters, shown as two groups of four. */
	code: string;
	expires_in: number;
	/** The CA certificate's SHA-256 (hex), for the link's `ca=`. */
	ca: string | null;
	ca_problem: string | null;
	allow_http: boolean;
}

export type PairingStatus =
	| { state: 'waiting'; expires_in: number }
	/** A device used it; its token is being issued. */
	| { state: 'pairing' }
	| { state: 'paired'; device: Device | null }
	| { state: 'expired' }
	| { state: 'cancelled'; why: 'failures' | 'insecure' | 'removed' }
	| { state: 'failed' };

export interface PairingSetup {
	/** The public certificate of the CA that signed the server's certificate, for Android apps. */
	ca_path: string | null;
	/** Pairing over plain http, for a network that is encrypted already. */
	allow_http: boolean;
}

export interface CaInfo {
	/** SHA-256 in lowercase hex, as in the pairing link. */
	fingerprint: string;
	/** As `openssl x509 -noout -fingerprint -sha256` prints it. */
	sha256: string;
}

export interface PairingSetupResponse {
	setup: PairingSetup;
	ca: CaInfo | null;
	/** Why the CA certificate can't be used, if it can't. */
	ca_problem: string | null;
}

/** How this browser is signed in (`GET /api/session`). */
export interface Access {
	/** A password login, an app's token, or a session traded for one. */
	via: 'password' | 'token' | 'token_session';
	scope: 'manage' | 'sync';
	device: number | null;
}

/** What uploads take, and where they go (`GET /api/uploads`). */
export interface UploadsInfo {
	/** The uploads folder as coversyncd sees it; null without an Inbox folder. */
	folder: FolderStatus | null;
	/** The chunk size to send, in bytes. */
	chunk_size: number;
	/** The file extensions uploads take, lowercase. */
	extensions: string[];
	max_files: number;
}

/** An upload under way on the server, with each file's progress. */
export interface ServerUpload {
	id: string;
	folder: string;
	size: number;
	received: number;
	files: {
		index: number;
		path: string;
		size: number;
		/** The bytes the server has, as `[start, end)` pairs. */
		ranges: [number, number][];
	}[];
}

/** A finished upload: the folder it went into, numbered if the name was taken. */
export interface FinishedUpload {
	folder: string;
	/** Where it is on the server. */
	path: string;
}

export class ApiError extends Error {
	constructor(
		public status: number,
		message: string,
		/** The error response's JSON, `{}` if it had none. */
		public body: Record<string, unknown> = {}
	) {
		super(message);
	}

	/** The API's `code` for it, when it gave one. */
	get code(): string | undefined {
		return typeof this.body.code === 'string' ? this.body.code : undefined;
	}
}

/** A tag save refused because the album had changed since the version it was made on: nothing was saved. */
export class TagConflict extends ApiError {
	constructor(
		message: string,
		/** The album as it is now; null when neither its tracks nor its id name one any more. */
		public album: AlbumDetail | null
	) {
		super(409, message);
	}
}

let unauthorized: (code: string) => void = () => {};
/** Hear about every 401: the session has ended, or there was none. */
export function onUnauthorized(handler: (code: string) => void) {
	unauthorized = handler;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
	const res = await fetch(BASE + path, init);
	if (!res.ok) {
		const body = await res.json().catch(() => ({}));
		if (res.status === 401) unauthorized(body.code ?? 'login_required');
		throw new ApiError(res.status, body.error ?? res.statusText, body);
	}
	return res.status === 202 || res.status === 204 ? (undefined as T) : res.json();
}

/**
 * Send one chunk of an upload: `body` into file `index` from byte `offset`.
 * By XMLHttpRequest, which, unlike fetch, tells how much of the body has
 * gone (`progress`). The browser reads a file's slice from disk as it sends
 * it. Throws an ApiError, with status 0 when the request got no answer (the
 * connection dropped, or it was aborted through `signal`).
 */
export function sendChunk(
	id: string,
	index: number,
	offset: number,
	body: Blob,
	progress: (sent: number) => void,
	signal: AbortSignal
): Promise<void> {
	return new Promise((resolve, reject) => {
		const lost = () => reject(new ApiError(0, 'The connection to coversyncd was lost.'));
		if (signal.aborted) return lost();
		const xhr = new XMLHttpRequest();
		xhr.open(
			'PUT',
			`${BASE}/api/uploads/${encodeURIComponent(id)}/files/${index}?offset=${offset}`
		);
		xhr.upload.onprogress = (e) => progress(e.loaded);
		xhr.onload = () => {
			if (xhr.status >= 200 && xhr.status < 300) return resolve();
			let answer: Record<string, unknown> = {};
			try {
				answer = JSON.parse(xhr.responseText);
			} catch {}
			const code = typeof answer.code === 'string' ? answer.code : 'login_required';
			if (xhr.status === 401) unauthorized(code);
			const message = typeof answer.error === 'string' ? answer.error : xhr.statusText;
			reject(new ApiError(xhr.status, message, answer));
		};
		xhr.onerror = xhr.onabort = xhr.ontimeout = lost;
		signal.addEventListener('abort', () => xhr.abort(), { once: true });
		xhr.send(body);
	});
}

const json = (method: string, body: unknown): RequestInit => ({
	method,
	headers: { 'content-type': 'application/json' },
	body: JSON.stringify(body)
});

export const api = {
	session: () => request<Access>('/api/session'),
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
	 * `refresh` starts over; `fresh` also asks AcoustID and MusicBrainz again
	 * instead of using their saved answers.
	 */
	identify: (
		album_id: string,
		release_id?: string,
		refresh = false,
		romanize?: boolean,
		fresh = false
	) =>
		request<IdentifyState>('/api/identify', {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ album_id, release_id, refresh, romanize, fresh })
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
	/** `uploads`: check it as an uploads folder, which only the folder holding it can make. */
	checkFolder: (path: string, uploads = false) =>
		request<FolderStatus | null>(
			`/api/fs/check?path=${encodeURIComponent(path)}${uploads ? '&uploads=true' : ''}`
		),
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
	syncSetup: () => request<SyncSetupResponse>('/api/sync/setup'),
	saveSyncSetup: (setup: SyncSetup) =>
		request<SyncSetupResponse>('/api/sync/setup', {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(setup)
		}),
	devices: () => request<DevicesResponse>('/api/devices'),
	/** Rename or re-scope a device (needs a password login). */
	updateDevice: (id: number, change: { name?: string; scope?: Scope }) =>
		request<Device>(`/api/devices/${id}`, json('PUT', change)),
	revokeDevice: (id: number) => request<Device>(`/api/devices/${id}/revoke`, { method: 'POST' }),
	removeDevice: (id: number) => request<void>(`/api/devices/${id}`, { method: 'DELETE' }),
	saveDeviceSync: (id: number, settings: DeviceSettings) =>
		request<DeviceSync>(`/api/devices/${id}/sync/settings`, json('PUT', settings)),
	/** A password login reads any device's; an app only its own. */
	deviceSyncState: (id: number) => request<DeviceSyncState>(`/api/devices/${id}/sync/state`),
	deviceSyncAction: (id: number, action: SyncAction) =>
		request<void>(`/api/devices/${id}/sync/action`, json('POST', { action })),
	/** Add device: a code for a device to pair with, replacing `replace`'s entry if given. */
	newPairing: (name: string, scope: Scope, replace: number | null) =>
		request<NewPairing>('/api/pairings', json('POST', { name, scope, replace })),
	pairing: (id: string) => request<PairingStatus>(`/api/pairings/${encodeURIComponent(id)}`),
	/** The dialog closed: its code stops working. */
	closePairing: (id: string, keepalive = false) =>
		request<void>(`/api/pairings/${encodeURIComponent(id)}`, { method: 'DELETE', keepalive }),
	pairingSetup: () => request<PairingSetupResponse>('/api/pairing/setup'),
	savePairingSetup: (setup: PairingSetup) =>
		request<PairingSetupResponse>('/api/pairing/setup', json('PUT', setup)),
	/** Check a CA certificate path as it's typed; null for an empty one. */
	checkCa: (path: string) =>
		request<CaInfo | null>(`/api/pairing/check-ca?path=${encodeURIComponent(path)}`),
	unhideAll: () =>
		request<{ unhidden: number; hidden: HiddenStats }>('/api/inbox/unhide', { method: 'POST' }),
	uploads: () => request<UploadsInfo>('/api/uploads'),
	startUpload: (folder: string, files: { path: string; size: number }[]) =>
		request<ServerUpload>('/api/uploads', json('POST', { folder, files })),
	upload: (id: string) => request<ServerUpload>(`/api/uploads/${encodeURIComponent(id)}`),
	finishUpload: (id: string) =>
		request<FinishedUpload>(`/api/uploads/${encodeURIComponent(id)}/finish`, { method: 'POST' }),
	/** The Inbox albums with files where an upload went (finishing's `path`), as the last scan found them. */
	uploadAlbums: (path: string) =>
		request<{ albums: string[] }>(`/api/uploads/albums?path=${encodeURIComponent(path)}`),
	/** `keepalive`: sent even as the page goes away. */
	cancelUpload: (id: string, keepalive = false) =>
		request<void>(`/api/uploads/${encodeURIComponent(id)}`, { method: 'DELETE', keepalive }),
	albumTags: (id: string) => request<AlbumTags>(`/api/albums/${id}/tags`),
	/** Throws TagConflict when `body.version` is no longer the album's. */
	writeTags: (id: string, body: TagWrite) =>
		request<TagWriteResult>(`/api/albums/${id}/tags`, {
			method: 'PUT',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(body)
		}).catch((e) => {
			if (e instanceof ApiError && e.status === 409)
				throw new TagConflict(e.message, (e.body.album as AlbumDetail | null) ?? null);
			throw e;
		})
};

/** An album's cover, at least `size` px. Its cover version lets the browser keep it until the art changes. */
export const coverUrl = (album: Pick<Album, 'id' | 'cover_version'>, size = 512) =>
	`${BASE}/api/albums/${album.id}/cover?size=${size}${album.cover_version ? `&v=${album.cover_version}` : ''}`;
/** The album's art at full size. */
export const fullCoverUrl = (id: string) => `${BASE}/api/albums/${id}/cover/full`;
/** MusicBrainz's cover, once `api.musicbrainzArt` has downloaded it. */
export const musicbrainzArtUrl = (release_id: string) =>
	`${BASE}/api/musicbrainz/art/${release_id}/image`;
export const streamUrl = (trackId: number) => `${BASE}/api/tracks/${trackId}/stream`;

/** What the desktop app's proxy adds to `/api/hello`'s answer; both null in a browser. */
export interface Hello {
	/** `Coversync-Server`: the server the proxy stands for. */
	server: string | null;
	/** `Coversync-App`: the versions of the app's interface the app speaks (desktop.svelte.ts). */
	app: string | null;
}

let greeting: Promise<Hello> | undefined;
/**
 * Read `/api/hello` once. Fails when it can't be read (the proxy's 502 while
 * the server is down, say), and is asked again next time.
 */
export function hello(): Promise<Hello> {
	if (!greeting) {
		greeting = fetch(`${BASE}/api/hello`, { cache: 'no-store' }).then((res) => {
			if (!res.ok) throw new Error(`/api/hello: ${res.status}`);
			return { server: res.headers.get('coversync-server'), app: res.headers.get('coversync-app') };
		});
		greeting.catch(() => (greeting = undefined));
	}
	return greeting;
}

/**
 * The address other devices reach coversync at: this page's own, with its
 * path prefix. In the desktop app the page comes from a proxy on 127.0.0.1,
 * which names the server it stands for. Fails when `hello` does.
 */
export const serverAddress = (): Promise<string> =>
	hello().then(({ server }) =>
		server && URL.canParse(server) ? server : `${location.origin}${BASE}/`
	);
