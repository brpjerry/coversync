import { describe, expect, it } from 'vitest';
import type { Album } from './api';
import { matcher, NONE, tagValues } from './filters';

const album = (kind: string, values?: string[]) =>
	({ filter_values: values ? { [kind]: values } : {} }) as unknown as Album;

describe('None', () => {
	const albums = [
		album('collection', ['B']),
		album('collection', []),
		album('collection', ['A', 'B']),
		album('collection', []),
		album('collection', ['None'])
	];

	it('counts the albums without a value, last, apart from a value named None', () => {
		expect(tagValues(albums, 'collection')).toEqual([
			{ value: 'A', count: 1 },
			{ value: 'B', count: 2 },
			{ value: 'None', count: 1 },
			{ value: NONE, count: 2 }
		]);
	});

	it('picks the albums without a value, beside picked values', () => {
		const pick = (values: string[]) =>
			albums.filter(matcher({ collection: values }, ['collection']));
		expect(pick([NONE])).toEqual([albums[1], albums[3]]);
		expect(pick([NONE, 'A'])).toEqual(albums.slice(1, 4));
		expect(pick(['None'])).toEqual([albums[4]]);
	});

	it("leaves out albums loaded before the tag's values were asked for", () => {
		expect(tagValues([album('collection')], 'collection')).toEqual([]);
	});

	it('goes last among years too, after year 0', () => {
		const years = [album('date', []), album('date', ['0']), album('date', ['2004'])];
		expect(tagValues(years, 'date').map((t) => t.value)).toEqual(['2004', '0', NONE]);
	});
});
