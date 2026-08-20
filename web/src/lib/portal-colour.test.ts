import { describe, expect, it } from 'vitest';
import { contrast, legible, luminance, parseHex } from './portal-colour';

/** The pair a portal would actually render, as a contrast ratio. */
function ratio(accent: string): number {
	const { background, ink } = legible(accent);
	const b = parseHex(background);
	const i = parseHex(ink);
	if (!b || !i) throw new Error('derived colours must parse');
	return contrast(b, i);
}

describe('legible', () => {
	it('keeps a brand colour that already works', () => {
		// The app's own blue with white: comfortably over AA, so nothing should move.
		const { background, ink } = legible('#2563eb');
		expect(background).toBe('#2563eb');
		expect(ink).toBe('#ffffff');
	});

	it('fixes the orange the browser suite failed on', () => {
		// `#ff6600` with white is 2.93:1. The accent is the tenant's, so the *pair* has to change rather than
		// the colour being rejected.
		expect(contrast(parseHex('#ff6600')!, parseHex('#ffffff')!)).toBeLessThan(4.5);
		expect(ratio('#ff6600')).toBeGreaterThanOrEqual(4.5);
	});

	it('handles a mid-tone, where neither ink works on its own', () => {
		// The case that makes step 2 necessary: grey manages under 4:1 with white *and* with near-black.
		const grey = parseHex('#808080')!;
		expect(contrast(grey, parseHex('#ffffff')!)).toBeLessThan(4.5);
		expect(contrast(grey, parseHex('#111827')!)).toBeLessThan(4.5);
		expect(ratio('#808080')).toBeGreaterThanOrEqual(4.5);
	});

	it('meets AA for every hue at full saturation', () => {
		// Not a spot check: a tenant picks whatever they like, and the guarantee is for all of it.
		for (let hue = 0; hue < 360; hue += 15) {
			const accent = hslHex(hue, 100, 50);
			expect(ratio(accent), `hue ${hue} (${accent})`).toBeGreaterThanOrEqual(4.5);
		}
	});

	it('meets AA across the lightness range too', () => {
		for (let lightness = 5; lightness <= 95; lightness += 5) {
			const accent = hslHex(210, 80, lightness);
			expect(ratio(accent), `lightness ${lightness} (${accent})`).toBeGreaterThanOrEqual(4.5);
		}
	});

	it('chooses dark ink on a pale accent and light ink on a deep one', () => {
		expect(legible('#fef08a').ink).toBe('#111827');
		expect(legible('#1e1b4b').ink).toBe('#ffffff');
	});

	it('opens with a default rather than throwing on a typo', () => {
		// A portal with a bad colour should still open. Falling back beats a page that does not render.
		expect(legible('not a colour')).toEqual({ background: '#2563eb', ink: '#ffffff' });
		expect(legible('')).toEqual({ background: '#2563eb', ink: '#ffffff' });
	});

	it('agrees with the WCAG reference values', () => {
		// Anchors, so a refactor of the maths cannot drift quietly.
		expect(luminance(parseHex('#ffffff')!)).toBeCloseTo(1, 5);
		expect(luminance(parseHex('#000000')!)).toBeCloseTo(0, 5);
		expect(contrast(parseHex('#ffffff')!, parseHex('#000000')!)).toBeCloseTo(21, 4);
	});
});

/** An HSL colour as hex, for sweeping the space in the two cases above. */
function hslHex(h: number, s: number, l: number): string {
	const sat = s / 100;
	const light = l / 100;
	const c = (1 - Math.abs(2 * light - 1)) * sat;
	const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
	const m = light - c / 2;
	const [r, g, b] =
		h < 60
			? [c, x, 0]
			: h < 120
				? [x, c, 0]
				: h < 180
					? [0, c, x]
					: h < 240
						? [0, x, c]
						: h < 300
							? [x, 0, c]
							: [c, 0, x];
	const hex = (n: number) =>
		Math.round((n + m) * 255)
			.toString(16)
			.padStart(2, '0');
	return `#${hex(r)}${hex(g)}${hex(b)}`;
}
