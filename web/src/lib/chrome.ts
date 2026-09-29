/**
 * Whether a route is outside the application's own chrome.
 *
 * One definition, because two consumers have to agree about it and they fail differently. `Nav.svelte` uses it
 * to render nothing, and `+layout.svelte` uses it to drop the rail's margin — so a copy that drifted would
 * either indent a page with no rail beside it, or put the application's navigation on a page meant for
 * somebody else's customers.
 *
 * The public addresses, which the browser suite caught once already: Q.14's named portals live under
 * `/portal/`, a share link under `/share/`, and `/tour` is the product and documentation front door. Adding a
 * route without adding it to the predicate puts the whole operator shell on a page whose visitor has no account
 * and nothing to navigate to.
 */
const OUTSIDE = ['/share/', '/portal/', '/tour'];

export function isPublicRoute(pathname: string, base = ''): boolean {
	const appPath = base && pathname.startsWith(base) ? pathname.slice(base.length) || '/' : pathname;
	return OUTSIDE.some((prefix) => appPath.startsWith(prefix));
}
