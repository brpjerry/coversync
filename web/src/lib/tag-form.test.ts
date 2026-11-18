import { describe, expect, it } from 'vitest';
import {
	changeCount,
	changedElsewhere,
	edits,
	invalid,
	otherTags,
	setAll,
	shared,
	withKept,
	type Values
} from './tag-form';

const ids = [1, 2];
const album = (): Values => ({
	1: { title: 'One', date: '2010', genre: 'Idol', label: 'Lantis' },
	2: { title: 'Two', date: '2010.08.25', genre: 'Anime', label: 'Lantis' }
});

describe('album-wide fields', () => {
	it('show the value the tracks share, or none when they differ', () => {
		expect(shared(album(), ids, 'label')).toBe('Lantis');
		expect(shared(album(), ids, 'date')).toBeNull();
		expect(shared(album(), ids, 'composer')).toBe('');
	});

	it('set every track, and emptied keep each track’s own where they differed', () => {
		const base = album();
		const form = structuredClone(base);
		setAll(form, base, ids, 'date', '2010-08-25');
		expect([form[1].date, form[2].date]).toEqual(['2010-08-25', '2010-08-25']);
		setAll(form, base, ids, 'date', '');
		expect([form[1].date, form[2].date]).toEqual(['2010', '2010.08.25']);
		// Where they agreed, emptying clears.
		setAll(form, base, ids, 'label', '');
		expect([form[1].label, form[2].label]).toEqual(['', '']);
	});
});

describe('edits', () => {
	it('are each track’s changed kinds, an emptied one as ""', () => {
		const base = album();
		const form = structuredClone(base);
		setAll(form, base, ids, 'genre', 'Anime');
		form[1].label = '';
		form[2].catalog_number = 'LZM-2027';
		expect(edits(form, base, ids)).toEqual({
			1: { genre: 'Anime', label: '' },
			2: { catalog_number: 'LZM-2027' }
		});
	});

	it('count a kind set alike on every track once, else per track edited', () => {
		const base = album();
		const form = structuredClone(base);
		setAll(form, base, ids, 'genre', 'Anime');
		expect(changeCount(form, base, ids)).toBe(1);
		// Any kind: a mood added to the album is one change.
		setAll(form, base, ids, 'mood', 'Happy');
		expect(changeCount(form, base, ids)).toBe(2);
		form[1].title = 'Uno';
		form[2].title = 'Dos';
		expect(changeCount(form, base, ids)).toBe(4);
		// One track's date: one.
		form[1].date = '2010-08-25';
		expect(changeCount(form, base, ids)).toBe(5);
	});
});

describe('invalid', () => {
	it('refuses what the server would', () => {
		expect(invalid('track_total', '13')).toBe(false);
		expect(invalid('track_number', '3/12')).toBe(false);
		expect(invalid('disc_total', 'two')).toBe(true);
		expect(invalid('date', '2010-08-25')).toBe(false);
		expect(invalid('date', '2010.08.25')).toBe(true);
		expect(invalid('label', 'anything at all')).toBe(false);
		expect(invalid('date', '')).toBe(false);
	});
});

describe('kept drafts', () => {
	it('go back on the tracks still here', () => {
		const kept = {
			base: album(),
			edits: { 1: { genre: 'Anime' }, 9: { title: 'Gone' } },
			paths: {}
		};
		const now = withKept(album(), kept, (id) => id !== 9);
		expect(now[1].genre).toBe('Anime');
		expect(now[9]).toBeUndefined();
	});

	it('say what changed elsewhere since, and what it was', () => {
		const kept = { base: album(), edits: {}, paths: {} };
		const now = album();
		now[2].label = 'Lantis Company, Limited';
		now[3] = { title: 'Three' };
		expect(changedElsewhere(kept, now, (id) => id !== 3)).toEqual({
			2: { label: 'Lantis' },
			3: { title: null }
		});
	});
});

describe('otherTags', () => {
	it('show one value where every track has it, else each track’s', () => {
		const rows = otherTags([
			{
				id: 1,
				other: [
					['TITLE_JP', '僕ら'],
					['CDTOC', '1+96']
				]
			},
			{
				id: 2,
				other: [
					['TITLE_JP', '友情'],
					['CDTOC', '1+96']
				]
			}
		]);
		expect(rows).toEqual([
			{
				key: 'TITLE_JP',
				value: null,
				tracks: [
					{ id: 1, value: '僕ら' },
					{ id: 2, value: '友情' }
				]
			},
			{
				key: 'CDTOC',
				value: '1+96',
				tracks: [
					{ id: 1, value: '1+96' },
					{ id: 2, value: '1+96' }
				]
			}
		]);
		// On one track only: per track.
		expect(
			otherTags([
				{ id: 1, other: [['X', 'y']] },
				{ id: 2, other: [] }
			])[0].value
		).toBeNull();
	});
});
