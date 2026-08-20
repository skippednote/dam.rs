/**
 * Making a tenant's brand colour legible (Q.14).
 *
 * A portal's accent arrives as data: whatever hex the organisation gave. That is the point — it is their page —
 * and it collides with an obligation the page cannot negotiate away, which is that text on a button has to meet
 * WCAG AA's 4.5:1. White on `#ff6600` is 2.93:1, and the browser suite failed on exactly that before this
 * existed.
 *
 * So the button's colours are *derived* rather than chosen:
 *
 * 1. Pick the ink — white or near-black — whichever contrasts more with the accent.
 * 2. If that pair still falls short (and for mid-tones both do: `#808080` manages under 4:1 either way), move the
 *    background away from the ink in steps until it passes.
 *
 * The result stays recognisably the brand, because step 2 only runs when it must and stops as soon as it can. The
 * raw accent is still used where contrast does not apply — a rule, a border, a focus ring.
 */

/** Near-black rather than black: `#111827` is the app's own darkest ink, and pure black on a colour looks harsh. */
export const DARK_INK = '#111827';
export const LIGHT_INK = '#ffffff';

/** WCAG AA for body text. The one number in here that is not a choice. */
const AA = 4.5;

/** How far to move the background per step, and how many steps to try before giving up and using the extreme. */
const STEP = 0.06;
const MAX_STEPS = 12;

type Rgb = { r: number; g: number; b: number };

/** Parses `#rrggbb`. Returns `null` for anything else — a caller then keeps its default rather than guessing. */
export function parseHex(hex: string): Rgb | null {
	const match = /^#([0-9a-f]{6})$/i.exec(hex.trim());
	if (!match) return null;
	const value = Number.parseInt(match[1], 16);
	return { r: (value >> 16) & 0xff, g: (value >> 8) & 0xff, b: value & 0xff };
}

function toHex({ r, g, b }: Rgb): string {
	const clamp = (n: number) => Math.max(0, Math.min(255, Math.round(n)));
	return `#${[clamp(r), clamp(g), clamp(b)].map((n) => n.toString(16).padStart(2, '0')).join('')}`;
}

/** Relative luminance, per WCAG 2. */
export function luminance(colour: Rgb): number {
	const channel = (raw: number) => {
		const c = raw / 255;
		return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
	};
	return 0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b);
}

/** Contrast ratio between two colours, per WCAG 2. */
export function contrast(a: Rgb, b: Rgb): number {
	const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x);
	return (light + 0.05) / (dark + 0.05);
}

/** Moves a colour towards black (`amount` < 0) or white (> 0) by a fraction of the remaining distance. */
function shift(colour: Rgb, amount: number): Rgb {
	const target = amount > 0 ? 255 : 0;
	const weight = Math.abs(amount);
	return {
		r: colour.r + (target - colour.r) * weight,
		g: colour.g + (target - colour.g) * weight,
		b: colour.b + (target - colour.b) * weight
	};
}

/** A button's background and text, derived from a tenant's accent so the pair always meets AA. */
export type Legible = { background: string; ink: string };

/**
 * The legible pair for an accent.
 *
 * An unparseable accent falls back to the app's own blue rather than throwing: a portal with a typo in its colour
 * should still open.
 */
export function legible(accent: string): Legible {
	const parsed = parseHex(accent) ?? parseHex('#2563eb');
	// Unreachable — the fallback is a literal this module owns — but stated rather than asserted, because a panic
	// in a colour function would take down a page for a cosmetic reason.
	if (!parsed) return { background: '#2563eb', ink: LIGHT_INK };

	const dark = parseHex(DARK_INK) ?? { r: 17, g: 24, b: 39 };
	const light = { r: 255, g: 255, b: 255 };
	// Whichever ink contrasts more. For a mid-tone that is a coin toss and either way needs step 2.
	const useLight = contrast(parsed, light) >= contrast(parsed, dark);
	const ink = useLight ? light : dark;

	let background = parsed;
	for (let step = 0; step <= MAX_STEPS; step += 1) {
		if (contrast(background, ink) >= AA) break;
		// Away from the ink: darker under white text, lighter under dark text.
		background = shift(parsed, (useLight ? -1 : 1) * STEP * (step + 1));
	}

	return {
		background: toHex(background),
		ink: useLight ? LIGHT_INK : DARK_INK
	};
}
