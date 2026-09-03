import { error } from '@sveltejs/kit';
import { DOCS, isDocSlug } from '$lib/docs/content';

export function load({ params, url }) {
	const slug = (params.slug ?? url.pathname.split('/').filter(Boolean).at(-1) ?? '').replace(
		/\/+$/,
		''
	);
	if (!isDocSlug(slug) || slug === 'getting-started') {
		error(404, 'Documentation page not found');
	}

	return {
		slug,
		title: DOCS[slug].nav
	};
}
