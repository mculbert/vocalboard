import { describe, it, expect, afterEach } from 'vitest';
import { mockIPC, clearMocks } from '@tauri-apps/api/mocks';
import {
	exportMixed,
	exportTrack,
	exportTranscript,
	getAppInfo,
	newProject,
	openProject,
	pause,
	pingSidecar,
	playFrom,
	saveSnapshotNow,
	stop
} from './commands.js';
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

describe('playFrom', () => {
	it('invokes play_from with start_sample and a null end_sample by default', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'play_from') {
				capturedArgs = args;
				return null;
			}
		});
		await playFrom(48000n);
		expect(capturedArgs).toEqual({ params: { start_sample: 48000n, end_sample: null } });
	});

	it('forwards an explicit end_sample', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'play_from') {
				capturedArgs = args;
				return null;
			}
		});
		await playFrom(0n, 96000n);
		expect(capturedArgs).toEqual({ params: { start_sample: 0n, end_sample: 96000n } });
	});
});

describe('pause / stop', () => {
	it('invokes pause with empty params', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'pause') {
				capturedArgs = args;
				return null;
			}
		});
		await pause();
		expect(capturedArgs).toEqual({ params: {} });
	});

	it('invokes stop with empty params', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'stop') {
				capturedArgs = args;
				return null;
			}
		});
		await stop();
		expect(capturedArgs).toEqual({ params: {} });
	});
});

describe('exportTrack', () => {
	it('defaults format to flac and mono to false', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'export_track') {
				capturedArgs = args;
				return null;
			}
		});
		await exportTrack(2, '/tmp/out.flac');
		expect(capturedArgs).toEqual({
			params: { track_id: 2, output_path: '/tmp/out.flac', format: 'flac', mono: false }
		});
	});

	it('forwards an explicit format and mono', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'export_track') {
				capturedArgs = args;
				return null;
			}
		});
		await exportTrack(1, '/tmp/out.wav', 'wav', true);
		expect(capturedArgs).toEqual({
			params: { track_id: 1, output_path: '/tmp/out.wav', format: 'wav', mono: true }
		});
	});
});

describe('exportMixed', () => {
	it('defaults format to flac and mono to false', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'export_mixed') {
				capturedArgs = args;
				return null;
			}
		});
		await exportMixed('/tmp/mix.flac');
		expect(capturedArgs).toEqual({
			params: { output_path: '/tmp/mix.flac', format: 'flac', mono: false }
		});
	});
});

describe('exportTranscript', () => {
	it('defaults format to vtt and include_cut_words to false', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'export_transcript') {
				capturedArgs = args;
				return null;
			}
		});
		await exportTranscript('/tmp/t.vtt');
		expect(capturedArgs).toEqual({
			params: { output_path: '/tmp/t.vtt', format: 'vtt', include_cut_words: false }
		});
	});

	it('forwards markdown format and include_cut_words', async () => {
		let capturedArgs: unknown;
		mockIPC((cmd, args) => {
			if (cmd === 'export_transcript') {
				capturedArgs = args;
				return null;
			}
		});
		await exportTranscript('/tmp/t.md', 'markdown', true);
		expect(capturedArgs).toEqual({
			params: { output_path: '/tmp/t.md', format: 'markdown', include_cut_words: true }
		});
	});
});
