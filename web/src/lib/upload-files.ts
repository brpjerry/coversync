// What to upload, from a drop or a file picker. Each dropped or picked
// folder is one upload, named after it, holding its files at their paths
// inside it (`CD1/01 Intro.flac`); loose files go together into one more.
// Files the server wouldn't take, hidden ones and kinds it doesn't accept,
// are left out and listed, so the rest can go.

export interface PickedFile {
	/** Inside the upload's folder, with / between folders. */
	path: string;
	file: Blob;
}

export interface Picked {
	/** The upload's folder name; '' for loose files, which the server names Upload. */
	folder: string;
	files: PickedFile[];
	/** Paths left out, a hidden folder as `name/`, relative to where the drop or pick began. */
	skipped: string[];
}

/** The parts of the File and Directory Entries API a drop gives (`webkitGetAsEntry`). */
export interface Entry {
	name: string;
	isFile: boolean;
	isDirectory: boolean;
}
interface FileEntry extends Entry {
	file(ok: (file: File) => void, fail: (error: DOMException) => void): void;
}
interface DirectoryEntry extends Entry {
	createReader(): {
		readEntries(ok: (entries: Entry[]) => void, fail: (error: DOMException) => void): void;
	};
}

const hidden = (name: string) => name.startsWith('.');

/** A file the server takes: no hidden names on its path, and one of `extensions`. */
function takes(path: string, extensions: ReadonlySet<string>) {
	const names = path.split('/');
	const name = names[names.length - 1];
	const dot = name.lastIndexOf('.');
	return !names.some(hidden) && dot > 0 && extensions.has(name.slice(dot + 1).toLowerCase());
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** In the order a file manager lists them, so CD1 goes before CD2, and 2 before 10. */
function sortFiles(files: PickedFile[]) {
	return files.sort((a, b) => collator.compare(a.path, b.path));
}

/** Every entry of a folder: browsers hand them over in batches (Chromium's are 100 long). */
async function readAll(dir: DirectoryEntry): Promise<Entry[]> {
	const reader = dir.createReader();
	const all: Entry[] = [];
	for (;;) {
		const batch = await new Promise<Entry[]>((ok, fail) => reader.readEntries(ok, fail));
		if (!batch.length) return all;
		all.push(...batch);
	}
}

const fileOf = (entry: FileEntry) => new Promise<File>((ok, fail) => entry.file(ok, fail));

/** The files under `dir`, with paths starting `prefix`; what's left out goes in `skipped`. */
async function walk(
	dir: DirectoryEntry,
	prefix: string,
	extensions: ReadonlySet<string>,
	skipped: string[]
): Promise<PickedFile[]> {
	const entries = await readAll(dir);
	// The folders' walks and the files' lookups all at once: each is a trip to
	// the browser's file thread.
	const found = await Promise.all(
		entries.map(async (entry): Promise<PickedFile[]> => {
			const path = prefix + entry.name;
			if (entry.isDirectory) {
				if (hidden(entry.name)) {
					skipped.push(`${path}/`);
					return [];
				}
				return walk(entry as DirectoryEntry, `${path}/`, extensions, skipped);
			}
			if (!entry.isFile) return [];
			if (!takes(path, extensions)) {
				skipped.push(path);
				return [];
			}
			return [{ path, file: await fileOf(entry as FileEntry) }];
		})
	);
	return found.flat();
}

/**
 * The uploads a drop makes, from its entries (`DataTransferItem.webkitGetAsEntry()`,
 * which must be called while the drop event is handled): one per folder, and
 * one for the loose files. Folders with nothing to upload give an upload with
 * no files, whose `skipped` says why.
 */
export async function fromEntries(
	entries: Entry[],
	extensions: ReadonlySet<string>
): Promise<Picked[]> {
	const loose: Picked = { folder: '', files: [], skipped: [] };
	const folders = await Promise.all(
		entries
			.filter((entry) => entry.isDirectory)
			.map(async (dir): Promise<Picked> => {
				const skipped: string[] = [];
				if (hidden(dir.name)) return { folder: dir.name, files: [], skipped: [`${dir.name}/`] };
				const files = await walk(dir as DirectoryEntry, '', extensions, skipped);
				return { folder: dir.name, files: sortFiles(files), skipped };
			})
	);
	const files = entries.filter((entry) => entry.isFile);
	loose.skipped = files.filter((f) => !takes(f.name, extensions)).map((f) => f.name);
	loose.files = sortFiles(
		await Promise.all(
			files
				.filter((f) => takes(f.name, extensions))
				.map(async (f) => ({ path: f.name, file: await fileOf(f as FileEntry) }))
		)
	);
	return loose.files.length || loose.skipped.length ? [...folders, loose] : folders;
}

/**
 * The uploads a file picker makes. With `webkitdirectory`, each file's
 * `webkitRelativePath` starts with the picked folder's name: that's the
 * upload's folder. Otherwise the files are loose.
 */
export function fromFiles(files: Iterable<File>, extensions: ReadonlySet<string>): Picked[] {
	const groups = new Map<string, Picked>();
	for (const file of files) {
		const relative = file.webkitRelativePath || file.name;
		const slash = relative.indexOf('/');
		const folder = slash < 0 ? '' : relative.slice(0, slash);
		const path = relative.slice(slash + 1);
		let group = groups.get(folder);
		if (!group) groups.set(folder, (group = { folder, files: [], skipped: [] }));
		if (takes(relative, extensions)) group.files.push({ path, file });
		else group.skipped.push(path);
	}
	return [...groups.values()].map((g) => ({ ...g, files: sortFiles(g.files) }));
}
