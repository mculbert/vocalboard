import { describe, it, expect, afterEach } from 'vitest';
import { mockIPC, clearMocks } from '@tauri-apps/api/mocks';
import { getAppInfo, pingSidecar } from './commands.js';

afterEach(() => {
	clearMocks();
});

describe('getAppInfo', () => {
	it('invokes get_app_info and returns the result', async () => {
		const fixture: AppInfoResult = { version: '0.1.0', sidecar_status: 'ready' };
		mockIPC((cmd) => {
			if (cmd === 'get_app_info') return fixture;
		});
		const result = await getAppInfo();
		expect(result).toEqual(fixture);
	});
});

describe('pingSidecar', () => {
	it('invokes ping_sidecar and returns pong: true', async () => {
		const fixture: PingResult = { pong: true };
		mockIPC((cmd) => {
			if (cmd === 'ping_sidecar') return fixture;
		});
		const result = await pingSidecar();
		expect(result.pong).toBe(true);
	});
});
