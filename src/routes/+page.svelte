<script lang="ts">
	import { onMount } from 'svelte';
	import * as m from '$lib/i18n/messages.js';
	import { getAppInfo, pingSidecar } from '$lib/ipc/commands.js';

	let appInfo = $state<AppInfoResult | null>(null);
	let pong = $state<boolean | null>(null);
	let loadError = $state<string | null>(null);

	onMount(async () => {
		try {
			const [info, pingResult] = await Promise.all([getAppInfo(), pingSidecar()]);
			appInfo = info;
			pong = pingResult.pong;
		} catch (e) {
			loadError = String(e);
		}
	});
</script>

<main class="flex min-h-screen flex-col items-center justify-center gap-6 p-8">
	<h1 class="text-3xl font-bold tracking-tight">{m.welcome_title()}</h1>

	{#if loadError}
		<p class="text-destructive" role="alert">{loadError}</p>
	{:else if appInfo === null}
		<p class="text-muted-foreground">{m.welcome_loading()}</p>
	{:else}
		<dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
			<dt class="font-medium">{m.label_version()}:</dt>
			<dd>{m.status_version({ version: appInfo.version })}</dd>

			<dt class="font-medium">{m.label_sidecar()}:</dt>
			<dd>
				{#if appInfo.sidecar_status === 'ready'}
					{m.status_sidecar_ready()}
				{:else if appInfo.sidecar_status === 'error'}
					{m.status_sidecar_error()}
				{:else}
					{m.status_sidecar_not_started()}
				{/if}
			</dd>

			<dt class="font-medium">{m.label_roundtrip()}:</dt>
			<dd>
				{#if pong === null}
					{m.welcome_loading()}
				{:else if pong}
					{m.status_pong()}
				{:else}
					{m.status_fail()}
				{/if}
			</dd>
		</dl>
	{/if}
</main>
