import { defineConfig } from '@playwright/test';

export default defineConfig({
	webServer: { command: 'npm run build && npm run preview', port: 4173 },
	testMatch: '**/*.e2e.{ts,js}',
	// This suite is flaky, and with no retries every flake is a red build.
	//
	// Measured rather than assumed — three full runs, and no test failed in more than one of
	// them:
	//
	// - an unchanged `main`, two workers: 4 of 410 (archival, browse ×2, people)
	// - a branch touching no frontend code, two workers: 3 of 410 (browse, collections,
	//   upload-profiles)
	// - the same branch, `--workers=1`: 1 of 410 (browse)
	//
	// All of them `toBeVisible` or `toContainText` timeouts in unrelated specs, so it is timing
	// rather than a broken assertion, and serialising helps without fixing it — which rules out
	// worker contention as the whole story. `browse.e2e.ts` appears in all three and is the
	// place to start looking.
	//
	// Retries do not fix that. What they do is make it legible: a test that passes on a second
	// attempt is reported as **flaky** and the build goes green, and one that fails three times
	// is a real failure worth reading. Without them a public repository's first-ever CI run is
	// red for a reason no reader can distinguish from "the tests do not pass". The underlying
	// flakiness is recorded in TASKS.md rather than left to be rediscovered here.
	retries: process.env.CI ? 2 : 0
});
