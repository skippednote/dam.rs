/**
 * The connection: where the API is, and the key to talk to it with.
 *
 * ## Why the key lives in `localStorage` and why that is stated rather than hidden
 *
 * The API authenticates a bearer token in a header, not a cookie. That is what makes the CORS policy on
 * the server side defensible — a cross-origin request without the header is anonymous, so there is no
 * ambient authority for a hostile origin to ride. The cost is that the token has to be somewhere
 * JavaScript can read, which means an XSS on this origin can exfiltrate it.
 *
 * A cookie would move that risk rather than remove it: `HttpOnly` protects the value from script but
 * hands every cross-origin request ambient authority, which is CSRF. The real fix is a session endpoint
 * issuing a short-lived, `HttpOnly`, `SameSite=Strict` cookie — that is a backend change, not a frontend
 * one, and it is not in this milestone. Until then the exposure is: a long-lived key readable by script
 * on this origin.
 *
 * So two things are deliberate. The key is never rendered back in full — only its prefix, which is what
 * an audit log shows too. And clearing it is one click, because a key that cannot be removed from a
 * shared browser is a key that stays there.
 */

const KEY_STORAGE = 'damrs.api_key';
const BASE_STORAGE = 'damrs.api_base';

/** Where `damd` listens by default in development. */
export const DEFAULT_BASE = 'http://127.0.0.1:8099';

/**
 * How much of a key is safe to show.
 *
 * The prefix is what `dam_global.api_keys.key_prefix` stores and what a log line carries, so showing
 * exactly that much is showing nothing the server has not already written down.
 */
export const VISIBLE_PREFIX = 12;

function read(name: string): string {
	// Guarded because this module is imported during SSR, where `localStorage` does not exist. Without
	// the guard the whole page fails to render rather than starting unauthenticated.
	if (typeof localStorage === 'undefined') return '';
	return localStorage.getItem(name) ?? '';
}

class Session {
	/** The bearer token. Empty means not connected. */
	key = $state(read(KEY_STORAGE));
	base = $state(read(BASE_STORAGE) || DEFAULT_BASE);

	get connected(): boolean {
		return this.key.length > 0;
	}

	/** The part of the key that may be displayed. See [`VISIBLE_PREFIX`]. */
	get visible(): string {
		if (!this.key) return '';
		return `${this.key.slice(0, VISIBLE_PREFIX)}…`;
	}

	connect(key: string, base: string) {
		// Trimmed, because a key pasted from a terminal carries a trailing newline and the hash of "k\n"
		// is not the hash of "k". The server trims too; doing it here as well means the stored value is
		// the one that works, rather than one that happens to survive a lenient server.
		this.key = key.trim();
		this.base = base.trim().replace(/\/+$/, '') || DEFAULT_BASE;
		if (typeof localStorage !== 'undefined') {
			localStorage.setItem(KEY_STORAGE, this.key);
			localStorage.setItem(BASE_STORAGE, this.base);
		}
	}

	disconnect() {
		this.key = '';
		if (typeof localStorage !== 'undefined') {
			localStorage.removeItem(KEY_STORAGE);
		}
	}
}

export const session = new Session();
