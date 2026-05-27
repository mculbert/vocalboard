import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { paraglideVitePlugin } from '@inlang/paraglide-js';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		tailwindcss(),
		paraglideVitePlugin({
			project: './project.inlang',
			outdir: './src/lib/i18n',
			// Single-locale English-only app for Phase 1.  No URL/cookie routing.
			strategy: ['baseLocale'],
		}),
		sveltekit(),
	],
});
