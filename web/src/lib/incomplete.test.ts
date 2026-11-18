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

	it('finds the disc of a gap numbered straight through, and adds missing discs', () => {
		const rows = withGaps(
			[
				[1, [t(1), t(2)]],
				[3, [t(5), t(6)]]
			],
			{ tracks: [gap(3, 4)], discs: [2] }
		);
		expect(rows).toEqual([
			[1, [{ track: t(1) }, { track: t(2) }, { from: 3, to: 4 }]],
			[2, null],
			[3, [{ track: t(5) }, { track: t(6) }]]
		]);
	});

	it('leaves a complete album as it is', () => {
		expect(withGaps([[1, [t(1)]]], undefined)).toEqual([[1, [{ track: t(1) }]]]);
	});
});
