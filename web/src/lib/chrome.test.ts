import { describe, expect, it } from 'vitest';
import { isPublicRoute } from './chrome';

describe('isPublicRoute', () => {
	it.each(['/share/token', '/portal/meridian.press-kit', '/tour', '/tour/architecture'])(
		'keeps %s outside the operator shell',
		(pathname) => {
			expect(isPublicRoute(pathname)).toBe(true);
		}
	);

	it.each(['/dam.rs/tour', '/dam.rs/tour/docs', '/dam.rs/share/token'])(
		'keeps %s outside the operator shell when the app has a Pages base',
		(pathname) => {
			expect(isPublicRoute(pathname, '/dam.rs')).toBe(true);
		}
	);

	it.each(['/', '/assets', '/settings'])('keeps %s inside the operator shell', (pathname) => {
		expect(isPublicRoute(pathname)).toBe(false);
	});
});
