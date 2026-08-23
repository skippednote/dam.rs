/**
 * The tenant's branding, loaded once for the shell.
 *
 * A module-level rune rather than a prop threaded through every page: the header renders on every route, and
 * passing branding down would mean every `+page.svelte` accepting a parameter it does not use. Loaded once and
 * shared, because it changes about once a year and is on screen constantly.
 *
 * Failure is silent on purpose. Branding is decoration — a library that would not render because a colour
 * could not be fetched would be a much worse failure than a blue accent, so a failed load leaves the defaults
 * and the settings screen is where an error would be worth showing.
 */
import { loadBranding, type Branding } from './client';
import { session } from './session.svelte';

/** The default, matching `dam_db::branding::DEFAULT_ACCENT` and the column default. */
export const DEFAULT_ACCENT = '#2563eb';

class SiteBranding {
	/** Empty until loaded. The shell falls back to "damrs" only while this is unknown. */
	name = $state('');
	accent = $state(DEFAULT_ACCENT);
	logoUrl = $state<string | null>(null);
	loaded = $state(false);

	/** Loads once. Safe to call from every mount; the guard makes the extra calls free. */
	async ensure(): Promise<void> {
		if (this.loaded || !session.connected) return;
		this.loaded = true;
		try {
			this.apply(await loadBranding());
		} catch {
			// Deliberately swallowed — see the module docs.
		}
	}

	/** Applies a freshly saved value, so the header updates without a reload. */
	apply(branding: Branding): void {
		this.name = branding.site_name;
		this.accent = branding.accent;
		this.logoUrl = branding.logo_url ?? null;
		this.loaded = true;
	}
}

export const branding = new SiteBranding();
