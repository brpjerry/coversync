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
		// Dates lofty reads whole, as the server's validate_kind (the same
		// table as tags.rs's DATES_READ_WHOLE and DATES_READ_IN_PART).
		const whole = [
			'2010',
			'2010-08',
			'2010-08-25',
			'2010.08.25',
			'2010.8.25',
			'20100825',
			'201008',
			'2010-08-25T12',
			'2010-08-25T12:30:45',
			'20100825T1230',
			'2010.08.25T12:30',
			'2010-08-25Z',
			'2010-08-25T',
			'2024-6-3T14:8:49',
			'2024- 6-03',
			'2024-6-03',
			'2024-06-03T1:02',
			'2024- 6',
			'2024-06- 3',
			'2024.6.03',
			'2024-06-03T 1:02',
			'2024-06-03T14:08:49Z',
			'2024-6-03Z',
			'202406',
			'2024-6 -03',
			'2024-06-03T1 :02',
			'2024-6 Z',
			'20240603T 1'
		];
		const inPart = [
			'2010-08-25 12:30',
			'2010-8-5',
			'2010/08/25',
			'2010-08-25T1230',
			'2010-08-25T12.30',
			'10-08-25',
			'summer',
			'2024-6',
			'2024-6-3',
			'2024-06-3',
			'2024-06-03T01:2',
			'2024-6-03T14:08:4',
			'2024-06 -03',
			'2010-08-25T01:30 PM'
		];
		for (const date of whole) expect(invalid('date', date), date).toBe(false);
		for (const date of inPart) expect(invalid('date', date), date).toBe(true);
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
				values: null,
				tracks: [
					{ id: 1, values: ['僕ら'] },
					{ id: 2, values: ['友情'] }
				]
			},
			{
				key: 'CDTOC',
				values: ['1+96'],
				tracks: [
					{ id: 1, values: ['1+96'] },
					{ id: 2, values: ['1+96'] }
				]
			}
		]);
		// On one track only: per track.
		expect(
			otherTags([
				{ id: 1, other: [['X', 'y']] },
				{ id: 2, other: [] }
			])[0].values
		).toBeNull();
	});

	it('keep a key held twice on one track as one row with both values', () => {
		const twice: [string, string][] = [
			['TITLE_JP', 'A'],
			['TITLE_JP', 'B']
		];
		// The same two, in the same order, on every track: shared.
		const same = otherTags([
			{ id: 1, other: twice },
			{ id: 2, other: twice }
		]);
		expect(same).toEqual([
			{
				key: 'TITLE_JP',
				values: ['A', 'B'],
				tracks: [
					{ id: 1, values: ['A', 'B'] },
					{ id: 2, values: ['A', 'B'] }
				]
			}
		]);
		// One row per track (unique ids for the list), however many values.
		const differ = otherTags([
			{ id: 1, other: twice },
			{ id: 2, other: [['TITLE_JP', 'A']] },
			{ id: 3, other: [] }
		]);
		expect(differ).toHaveLength(1);
		expect(differ[0].values).toBeNull();
		expect(differ[0].tracks.map((t) => t.id)).toEqual([1, 2]);
		expect(differ[0].tracks[0].values).toEqual(['A', 'B']);
	});
});
