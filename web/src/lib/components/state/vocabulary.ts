/**
 * The four-dimension state vocabulary.
 *
 * An asset carries four independent states at once, and a grid cell has to show all four without
 * becoming a colour puzzle. Each dimension gets a different perceptual channel:
 *
 * - **Tier → form.** Icon and border. Deliberately *not* colour, so it cannot compete with rights.
 * - **Rights → semantic colour.** The only dimension with legal consequence gets the loudest channel.
 * - **Provenance → neutral.** A missing credential is a fact about the file's history, not an alarm.
 * - **Confidence → magnitude.** It is a quantity, and quantities read as length.
 *
 * The state names mirror the database exactly — `assets.rights_state` and `assets.provenance_state`
 * CHECK these values — so a state the backend can produce always has somewhere to render. F.3 will
 * generate these types from the OpenAPI document; until then they are hand-written and the tests
 * assert they match the schema.
 */

export const RIGHTS_STATES = ['allowed', 'expiring', 'denied', 'unknown'] as const;
export type RightsState = (typeof RIGHTS_STATES)[number];

export const PROVENANCE_STATES = ['none', 'valid', 'invalid', 'untrusted'] as const;
export type ProvenanceState = (typeof PROVENANCE_STATES)[number];

export const TIERS = ['hot', 'cool', 'archive', 'restoring', 'restored'] as const;
export type Tier = (typeof TIERS)[number];

export interface TierMeta {
	label: string;
	/** Short description for a tooltip and the badge's accessible description. */
	detail: string;
	/** The form channel: a distinct glyph per tier. */
	icon: string;
	/** The form channel again: border treatment, so the tiers differ without colour. */
	border: 'solid' | 'dashed' | 'dotted' | 'double' | 'none';
	/** One neutral token for every tier — the assertion that keeps colour free for rights. */
	colorToken: string;
	/** Whether bytes need a restore request and a wait before they can be downloaded. */
	needsRestore: boolean;
}

const TIER_META: Record<Tier, TierMeta> = {
	hot: {
		label: 'Hot',
		detail: 'Available immediately.',
		icon: '●',
		border: 'solid',
		colorToken: 'state-neutral',
		needsRestore: false
	},
	cool: {
		label: 'Cool',
		detail: 'Available immediately; a retrieval fee applies.',
		icon: '◐',
		border: 'dashed',
		colorToken: 'state-neutral',
		needsRestore: false
	},
	archive: {
		label: 'Archived',
		detail: 'Searchable and previewable. The original needs a restore, which can take hours.',
		icon: '◌',
		border: 'dotted',
		colorToken: 'state-neutral',
		needsRestore: true
	},
	restoring: {
		label: 'Restoring',
		detail: 'A temporary copy of the original is being made.',
		icon: '◍',
		border: 'double',
		colorToken: 'state-neutral',
		needsRestore: true
	},
	restored: {
		label: 'Restored',
		detail: 'A temporary copy is available; it expires.',
		icon: '◉',
		border: 'none',
		colorToken: 'state-neutral',
		needsRestore: false
	}
};

export function tierMeta(tier: Tier): TierMeta {
	return TIER_META[tier];
}

export interface RightsMeta {
	label: string;
	detail: string;
	icon: string;
	/** Distinct per state: rights own the colour channel. */
	colorToken: string;
	/**
	 * Whether distribution is blocked. `unknown` blocks: the schema's AI gate is explicit that
	 * unevaluated rights are not permission, and a UI that renders unknown like allowed turns an
	 * unevaluated asset into an apparently cleared one.
	 */
	blocksDistribution: boolean;
}

const RIGHTS_META: Record<RightsState, RightsMeta> = {
	allowed: {
		label: 'Cleared',
		detail: 'Licensed for use.',
		icon: '✓',
		colorToken: 'state-rights-allowed',
		blocksDistribution: false
	},
	expiring: {
		label: 'Expiring',
		detail: 'Licensed, but the licence ends soon.',
		icon: '◔',
		colorToken: 'state-rights-expiring',
		blocksDistribution: false
	},
	denied: {
		label: 'Not licensed',
		detail: 'Distribution is blocked.',
		icon: '✕',
		colorToken: 'state-rights-denied',
		blocksDistribution: true
	},
	unknown: {
		label: 'Rights unknown',
		detail: 'Not yet evaluated. Treated as blocked until it is.',
		icon: '?',
		colorToken: 'state-rights-unknown',
		blocksDistribution: true
	}
};

export function rightsMeta(state: RightsState): RightsMeta {
	return RIGHTS_META[state];
}

export interface ProvenanceMeta {
	label: string;
	detail: string;
	icon: string;
	/** One neutral token for every state — provenance never shouts. */
	colorToken: string;
}

const PROVENANCE_META: Record<ProvenanceState, ProvenanceMeta> = {
	none: {
		label: 'No credential',
		detail: 'The file carries no content credential.',
		icon: '—',
		colorToken: 'state-neutral'
	},
	valid: {
		label: 'Credential verified',
		detail: 'The content credential is intact and its signer is trusted.',
		icon: '⛨',
		colorToken: 'state-neutral'
	},
	invalid: {
		label: 'Credential broken',
		detail: 'The credential does not match the file. Its history cannot be relied on.',
		icon: '⚟',
		colorToken: 'state-neutral'
	},
	untrusted: {
		label: 'Signer not trusted',
		detail: 'The credential is intact but its signer is not on a trust list.',
		icon: '◇',
		colorToken: 'state-neutral'
	}
};

export function provenanceMeta(state: ProvenanceState): ProvenanceMeta {
	return PROVENANCE_META[state];
}

/**
 * Normalises a confidence score to 0..1, or `null` when there is nothing to show.
 *
 * `null` is not zero. A tag with no score is usually one a human applied; drawing an empty bar would
 * claim the model was certain it was wrong, which inverts the meaning.
 */
export function clampConfidence(value: number | null | undefined): number | null {
	if (value === null || value === undefined || Number.isNaN(value)) return null;
	return Math.min(1, Math.max(0, value));
}

/** The value in words, for a screen reader and for anything printed in monochrome. */
export function confidenceLabel(value: number | null | undefined): string {
	const clamped = clampConfidence(value);
	if (clamped === null) return 'Confidence not scored';
	return `Confidence ${Math.round(clamped * 100)}%`;
}
