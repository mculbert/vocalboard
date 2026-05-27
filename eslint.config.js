import js from '@eslint/js';
import ts from 'typescript-eslint';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import svelteConfig from './svelte.config.js';

export default ts.config(
	js.configs.recommended,
	...ts.configs.recommended,
	// flat/recommended includes svelte/a11y-* rules and svelte/valid-compile.
	...svelte.configs['flat/recommended'],
	{
		languageOptions: {
			globals: { ...globals.browser, ...globals.node },
		},
	},
	{
		files: ['**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
		languageOptions: {
			parserOptions: {
				svelteConfig,
				parser: ts.parser,
			},
		},
		rules: {
			// TypeScript handles undefined-name checks in .svelte files; the JS
			// no-undef rule doesn't understand TypeScript global ambient types.
			'no-undef': 'off',
		},
	},
	{
		// Ignore generated and build outputs.
		// Note: no-hardcoded-string enforcement (D2 in conventions.md) is not yet
		// wired into CI.  No clean off-the-shelf ESLint rule exists for Svelte;
		// a custom rule or script is tracked in M6 (design/phase1.md).
		ignores: [
			'build/',
			'.svelte-kit/',
			'src-tauri/',
			'src/lib/i18n/',
			'src/lib/ipc/types.ts',
		],
	},
);
