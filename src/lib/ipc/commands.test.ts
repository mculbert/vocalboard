import { describe, it, expect, afterEach } from 'vitest';
import { mockIPC, clearMocks } from '@tauri-apps/api/mocks';
import { getAppInfo, newProject, openProject, pingSidecar, saveSnapshotNow } from './commands.js';
import type { AppInfoResult, NewProjectResult, OpenProjectResult, PingResult } from './types';

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

describe('newProject', () => {
	it('invokes new_project with params struct and returns sample_rate', async () => {
		const fixture: NewProjectResult = { sample_rate: 48000 };
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'new_project') {
				capturedArgs = args;
				return fixture;
			}
		});
		const result = await newProject('/tmp/test.vocalboard', 48000);
		expect(result).toEqual(fixture);
		expect(capturedArgs).toEqual({ params: { path: '/tmp/test.vocalboard', sample_rate: 48000 } });
	});
});

describe('openProject', () => {
	it('resolves with recovery: null when no rollback occurred', async () => {
		const fixture: OpenProjectResult = { missing_tracks: [], recovery: null };
		mockIPC((cmd) => {
			if (cmd === 'open_project') return fixture;
		});
		const result = await openProject('/tmp/test.vocalboard');
		expect(result.recovery).toBeNull();
		expect(result.missing_tracks).toEqual([]);
	});

	it('resolves with recovery report when rollback occurred', async () => {
		const fixture: OpenProjectResult = {
			missing_tracks: [3],
			recovery: { failed_row: 10n, snapshot_id: 2n }
		};
		mockIPC((cmd) => {
			if (cmd === 'open_project') return fixture;
		});
		const result = await openProject('/tmp/test.vocalboard');
		expect(result.recovery).not.toBeNull();
		expect(result.missing_tracks).toEqual([3]);
	});
});

describe('saveSnapshotNow', () => {
	it('invokes save_snapshot_now with empty params and resolves', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'save_snapshot_now') {
				capturedArgs = args;
				return null;
			}
		});
		await saveSnapshotNow();
		expect(capturedArgs).toEqual({ params: {} });
	});
});
