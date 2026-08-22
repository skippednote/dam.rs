<!--
	A skip link: the first focusable element on the page, jumping past the header and filter rail
	straight to the content.

	Positioned off-screen rather than `display: none`, deliberately. Hiding it until focus is the
	common implementation and it is wrong: `display: none` and `visibility: hidden` remove the element
	from the accessibility tree, so a screen-reader user browsing the page never encounters it and the
	affordance exists only for sighted keyboard users — who are not the people who need it most.
-->
<a
	href="#main-content"
	class="sr-only rounded-md bg-surface px-4 py-2 text-sm font-medium text-fg
	       ring-2 ring-accent focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-50"
>
	Skip to main content
</a>
