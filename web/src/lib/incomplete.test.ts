import { describe, expect, it } from 'vitest';
import { describeIncomplete, withGaps } from './incomplete';

const gap = (from: number, to: number, disc: number | null = null) => ({ disc, from, to });
const t = (track_no: number) => ({ track_no });

describe('describeIncomplete', () => {
	it('names the missing tracks of a one-disc album', () => {
		expect(describeIncomplete({ tracks: [gap(7, 7)], discs: [] })).toBe('Missing track 7');
		expect(describeIncomplete({ tracks: [gap(4, 4), gap(7, 9)], discs: [] })).toBe(
			'Missing tracks 4 and 7–9'
		);
		expect(describeIncomplete({ tracks: [gap(1, 1), gap(3, 3), gap(5, 6)], discs: [] })).toBe(
			'Missing tracks 1, 3 and 5–6'
		);
	});

	it('says which disc, in disc order, whole discs too', () => {
		expect(describeIncomplete({ tracks: [gap(4, 4, 1), gap(7, 9, 4)], discs: [3] })).toBe(
			'Missing disc 1 track 4, disc 3 and disc 4 tracks 7–9'
		);
		expect(describeIncomplete({ tracks: [], discs: [2] })).toBe('Missing disc 2');
		expect(describeIncomplete({ tracks: [gap(11, 20)], discs: [2] })).toBe(
			'Missing tracks 11–20 and disc 2'
		);
	});

	it('counts tracks an album-wide total asks for on no disc in particular', () => {
		expect(describeIncomplete({ tracks: [], discs: [], unplaced: 1 })).toBe(
			"Missing 1 track whose disc the tags don't say"
		);
		expect(describeIncomplete({ tracks: [gap(2, 2, 1)], discs: [], unplaced: 2 })).toBe(
			"Missing disc 1 track 2 and 2 more tracks whose disc the tags don't say"
		);
	});
});

describe('withGaps', () => {
	it('puts each run of missing numbers where it belongs', () => {
		const rows = withGaps([[1, [t(1), t(2), t(5)]]], { tracks: [gap(3, 4), gap(6, 8)], discs: [] });
		expect(rows).toEqual([
			[
				1,
				[{ track: t(1) }, { track: t(2) }, { from: 3, to: 4 }, { track: t(5) }, { from: 6, to: 8 }]
			]
		]);
		expect(withGaps([[1, [t(2)]]], { tracks: [gap(1, 1)], discs: [] })[0][1]).toEqual([
			{ from: 1, to: 1 },
			{ track: t(2) }
		]);
	});

	it('puts a gap numbered straight through on the missing disc between', () => {
		const rows = withGaps(
			[
				[1, [t(1), t(2)]],
				[3, [t(5), t(6)]]
			],
			{ tracks: [gap(3, 4)], discs: [2] }
		);
		expect(rows).toEqual([
			[1, [{ track: t(1) }, { track: t(2) }]],
			[2, [{ from: 3, to: 4 }]],
			[3, [{ track: t(5) }, { track: t(6) }]]
		]);
		// Disc 1 missing, before the first there.
		expect(
			withGaps(
				[
					[2, [t(3)]],
					[3, [t(4)]]
				],
				{ tracks: [gap(1, 2)], discs: [1] }
			)
		).toEqual([
			[1, [{ from: 1, to: 2 }]],
			[2, [{ track: t(3) }]],
			[3, [{ track: t(4) }]]
		]);
	});

	it('puts a gap inside a disc on it, and one between two discs there on the one before', () => {
		const discs: [number, { track_no: number }[]][] = [
			[1, [t(1), t(3)]],
			[2, [t(5), t(6)]]
		];
		expect(withGaps(discs, { tracks: [gap(2, 2), gap(4, 4)], discs: [] })).toEqual([
			[1, [{ track: t(1) }, { from: 2, to: 2 }, { track: t(3) }, { from: 4, to: 4 }]],
			[2, [{ track: t(5) }, { track: t(6) }]]
		]);
	});

	it('adds a missing disc with no gap on it as a disc of its own', () => {
		expect(
			withGaps(
				[
					[1, [t(1)]],
					[3, [t(1)]]
				],
				{ tracks: [], discs: [2] }
			)
		).toEqual([
			[1, [{ track: t(1) }]],
			[2, null],
			[3, [{ track: t(1) }]]
		]);
	});

	it('leaves a complete album as it is', () => {
		expect(withGaps([[1, [t(1)]]], undefined)).toEqual([[1, [{ track: t(1) }]]]);
	});
});
