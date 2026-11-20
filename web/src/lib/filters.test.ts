import { describe, expect, it } from 'vitest';
import type { Album } from './api';
import { matcher, NONE, tagValues } from './filters';

const album = (kind: string, values?: string[]) =>
	({ filter_values: values ? { [kind]: values } : {} }) as unknown as Album;

describe('None', () => {
	const albums = [
		album('collection', ['B']),
		album('collection'),
		album('collection', ['A', 'B']),
		album('collection', [])
	];

	it('counts the albums without a value, last', () => {
		expect(tagValues(albums, 'collection')).toEqual([
			{ value: 'A', count: 1 },
			{ value: 'B', count: 2 },
			{ value: NONE, count: 2 }
		]);
	});

	it('picks the albums without a value, beside picked values', () => {
		expect(albums.filter(matcher({ collection: [NONE] }, ['collection']))).toEqual([
			albums[1],
			albums[3]
		]);
		expect(albums.filter(matcher({ collection: [NONE, 'A'] }, ['collection']))).toEqual(
			albums.slice(1)
		);
	});

	it('goes last among years too', () => {
		const years = [album('date'), album('date', ['1999']), album('date', ['2004'])];
		expect(tagValues(years, 'date').map((t) => t.value)).toEqual(['2004', '1999', NONE]);
	});
});
