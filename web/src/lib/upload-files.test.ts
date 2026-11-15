import { describe, expect, it } from 'vitest';
import { fromEntries, fromFiles, type Entry } from './upload-files';

const EXTENSIONS = new Set(['flac', 'jpg', 'cue', 'log']);

/** A dropped file, as `webkitGetAsEntry()` gives it. */
function file(name: string): Entry {
	return {
		name,
		isFile: true,
		isDirectory: false,
		file: (ok: (f: File) => void) => ok(new File([name], name))
	} as Entry;
}

/** A dropped folder, handing its entries over in batches of 100, as Chromium does. */
function folder(name: string, entries: Entry[]): Entry {
	return {
		name,
		isFile: false,
		isDirectory: true,
		createReader() {
			let at = 0;
			return {
				readEntries(ok: (batch: Entry[]) => void) {
					const batch = entries.slice(at, at + 100);
					at += batch.length;
					setTimeout(() => ok(batch));
				}
			};
		}
	} as Entry;
}

const tracks = (n: number) =>
	Array.from({ length: n }, (_, i) => file(`${String(i + 1).padStart(3, '0')}.flac`));

describe('what a drop uploads', () => {
	it('keeps a folder’s structure, through every batch, and leaves out what uploads don’t take', async () => {
		const album = folder('Album', [
			folder('CD2', tracks(3)),
			folder('CD1', [...tracks(150), file('Thumbs.db')]),
			file('cover.jpg'),
			file('.DS_Store'),
			folder('.AppleDouble', [file('001.flac')])
		]);
		const [up, loose] = await fromEntries(
			[album, file('extra.flac'), file('notes.docx')],
			EXTENSIONS
		);

		expect(up.folder).toBe('Album');
		const paths = up.files.map((f) => f.path);
		expect(paths).toHaveLength(154);
		expect(paths.slice(0, 2)).toEqual(['CD1/001.flac', 'CD1/002.flac']);
		expect(paths).toContain('CD1/150.flac');
		expect(paths.slice(-4)).toEqual(['CD2/001.flac', 'CD2/002.flac', 'CD2/003.flac', 'cover.jpg']);
		expect(up.skipped.sort()).toEqual(['.AppleDouble/', '.DS_Store', 'CD1/Thumbs.db']);
		// Loose files go together, in a folder the server names.
		expect(loose).toMatchObject({ folder: '', skipped: ['notes.docx'] });
		expect(loose.files.map((f) => f.path)).toEqual(['extra.flac']);
	});

	it('takes a picked folder’s name from the paths the picker gives', () => {
		const picked = (path: string) => {
			const f = new File(['x'], path.split('/').pop()!);
			Object.defineProperty(f, 'webkitRelativePath', { value: path });
			return f;
		};
		const [up] = fromFiles(
			[picked('Album/CD2/01.flac'), picked('Album/CD1/01.flac'), picked('Album/.hidden/x.flac')],
			EXTENSIONS
		);
		expect(up.folder).toBe('Album');
		expect(up.files.map((f) => f.path)).toEqual(['CD1/01.flac', 'CD2/01.flac']);
		expect(up.skipped).toEqual(['.hidden/x.flac']);
	});
});
