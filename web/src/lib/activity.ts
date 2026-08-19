/**
 * How an activity line reads (Q.7, Q.10).
 *
 * The dashboard feed and one asset's history are the same sentence about different scopes — "somebody did something
 * to something, at a time" — and the API returns the same shape for both. So the phrasing lives here once. A second
 * copy in the history panel would be a second place to add a verb when a new event kind appears, and the one that
 * was forgotten would silently fall through to the plain form.
 */
import type { ActivityEntry } from '$lib/api/client';

/**
 * One activity line as a sentence.
 *
 * The kind decides the verb; anything unrecognised is reported as itself rather than dropped, because the events
 * column is deliberately open text — a future subsystem can record something without a migration, and hiding
 * activity is worse than phrasing it plainly.
 */
export function phrase(entry: ActivityEntry): string {
	const who = entry.actor?.name ?? 'Somebody';
	const what = entry.filename ?? 'an asset';
	switch (entry.kind) {
		case 'upload':
			return `${who} uploaded ${what}`;
		case 'edit':
			return `${who} edited ${what}`;
		case 'share':
			return `${who} shared ${what}`;
		case 'comment':
			// The context says public or private, and that is all it says — the words are not here.
			return entry.context && (entry.context as Record<string, unknown>).visibility === 'private'
				? `${who} left a private comment on ${what}`
				: `${who} commented on ${what}`;
		case 'download':
			return `${who} downloaded ${what}`;
		case 'delete':
			return `${who} deleted ${what}`;
		case 'restore':
			return `${who} asked for ${what} to be restored`;
		default:
			return `${who}: ${entry.kind} on ${what}`;
	}
}

/** Roughly how long ago, in the reader's own terms. Exact timestamps live in the `title` attribute. */
export function when(iso: string): string {
	const seconds = (Date.now() - new Date(iso).getTime()) / 1000;
	if (seconds < 90) return 'just now';
	if (seconds < 3600) return `${Math.round(seconds / 60)} min ago`;
	if (seconds < 86400) return `${Math.round(seconds / 3600)} h ago`;
	return new Date(iso).toLocaleDateString();
}
