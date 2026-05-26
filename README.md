# Vocalboard

An open-source, cross-platform desktop app for speech-forward audio editing, geared toward podcasters, audiobook narrators, and voiceover artists.

Vocalboard presents your recordings as an editable transcript. Import audio, get an automatically transcribed and speaker-labeled timeline, then edit by working with words — cut filler, mute noise, and rearrange speech — while the app handles the audio.

## Status

Early development. See [`design/`](design/) for the technical design document.

## Tech stack

- [Tauri 2](https://tauri.app/) + [Svelte 5](https://svelte.dev/) — desktop shell and UI
- Rust — project state, audio engine, playback
- Python sidecar — ML inference (transcription, diarization, enhancement, disfluency detection)

## License

[MIT](LICENSE)
