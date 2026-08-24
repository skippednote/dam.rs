/**
 * Whether a route is outside the application's own chrome.
 *
 * One definition, because two consumers have to agree about it and they fail differently. `Nav.svelte` uses it
 * to render nothing, and `+layout.svelte` uses it to drop the rail's margin — so a copy that drifted would
 * either indent a page with no rail beside it, or put the application's navigation on a page meant for
 * somebody else's customers.
 *
 * Both addresses, which the browser suite caught once already: Q.14's named portals live under `/portal/` and
 * a share link under `/share/`. Adding the second route without adding it to the predicate put the whole app
 * shell on a page whose visitor has no account and nothing to navigate to.
 */
const OUTSIDE = ['/share/', '/portal/'];

export function isPublicRoute(pathname: string): boolean {
	return OUTSIDE.some((prefix) => pathname.startsWith(prefix));
}
