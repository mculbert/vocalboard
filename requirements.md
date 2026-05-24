Vocalboard is an open-source, cross-platform desktop app for speech-forward (and transcript-based) audio editing and production, geared primarily for podcasters, but with additional use cases in audiobook production and voiceover narration. The user interface is easy for casual/novice users to navigate, with reasonable defaults, but also including progressive disclosure of the granular controls that provide the power a semi-pro user expects. The goal is not to cover every advanced feature of existing audio production software, but to provide a robust free and open-source “workhorse” alternative to paid software that will cover 90% of what novice-to-semi-pro users need (or even 100% of what 90% of target users would need), reducing users’ need to turn to more-specialized tools to only those particularly challenging audio editing tasks.

The app runs entirely locally; it should run on a consumer grade desktop or laptop, such as a relatively recent MacBook or non-Apple equivalent. The app is accessible by screen reader. Although the app is written in English, it makes use of internationalization practices to be ready for possible future translation of the UI.

This brainstorming document provides a provisional feature roadmap. 

## Phase 1: Minimum Viable Project

* I/O  
  * Import all the popular audio and video formats: wav, aiff, flac, alac, aac/m4a, mp3, ogg, webm, mo4, mov, mkv, H.264/H.265 (via system libraries), VP8/VP9, AV1, (others?) (move some formats to later phase if they require separate libraries/infrastructure)  
  * Export individual tracks, and complete rendered audio, cleaned transcript file (vtt, markdown, Word/RTF)  
  * Export mono tracks (collapse stereo input)  
  * Save a project file with: links to raw audio files, audio file alignment to master timeline, transcript with timing, edits (enhancement, word removal), audio parameters (e.g., target sampling rate, resampling parameters, etc.)  
  * Use sqlite for project file with autosave; write-only log of commands with periodic JSON serialization of the current state (linked to the point in the log at which it was serialized). At load, read the current state, and play forward any remaining commands in the log. Write out snapshot of current state after some time of inactivity (1s? 30s?) by copying in-memory data structure and sending serialization and output to a background thread.  
* Speech enhancement  
  * Automatic for low-quality transcription?  
    * If the average avg\_logprob across the entire file drops below \-1.0, or if more than 20\\% of individual segments fall below \-1.0, reject the transcript.  
    * Look for transcription despite no speech prob \> 0.6  
  * Slider for how much enhancement  
  * Aggressive chunking, 2-5s blocks overlapped by a few milliseconds  
  * Note: must determine the pipeline delay before wet/dry mixing  
  * High-Pass Filter (DC Offset / Rumble Removal) first?  
* Transcription  
  * Normalize loudness before sending to WhisperX  
  * With high-pass filter?  
* Track alignment  
  * Audalign at beginning and end, or minimize cross-correlation (via FFT) in speaking flags (at least three turns each, preferably more)  
  * Adjust for clock drift in the recordings (align beginning and end and spread the difference over the duration)  
  * For aligning more than two tracks, order the tracks in descending order of most speech time; then, align each track against the union of the preceding tracks  
  * Append tracks  
  * Note: Need to keep track of which tracks have been aligned  
* Speaker diarization  
  * Custom names for speakers  
  * Keep track of speaker embeddings after audio import  
  * Common speakers detected across tracks  
* Disfluency detection (filler words)  
  * For Gemma 3 1B: Four turns at a time, with two turns overlap across batches  
  * For Gemma 3 4B: \~2-3k words per batch, \~2 min overlap (\~300 words)  
  * For Gemma 3 12B: \~4-6k words per batch  
  * Note: words that overlap have to be muted, rather than cut  
* Word deletion/mute  
  * Button and hotkey to delete/undelete, mute/unmute  
  * Show deletion as strike through and grey, muting as grey in square brackets (don't double up the brackets for muted sounds)  
  * A cut may not be applied to a word that overlaps another turn; words that overlap with other turns can only be muted.  
  * Exception: overlapping turns may be cut in their entirety.  
  * Mute audio is replaced with the track’s room tone  
    * Note: room tone detected at import; store resampled room tone segment in the sqlite database  
    * Detect room tone by calculating RMS of 100ms-blocks; look for the 2s stretch (5-10s would be better) with the lowest cumulative RMS.  
    * Ensure that peak energy (absolute value of sample) is no more than 5 times the sample RMS and that the sd of the RMS of the 100ms blocks of the proposed segment is no more than 15% of the mean RMS.  
    * 100ms crossfade at head/tail to loop (of 500ms for longer source)  
    * If absolutely necessary, search for 100-300ms quiet segments and stitch together with 50ms crossfades.  
  * Look for zero-crossing in the 20ms before start and 20ms after end of the speech track (cut all other tracks at same location), with 2ms cross-fade. Acceptable zero-crossing has local energy (RMS) \< max(0.001, min(2\*room tone energy, 0.0316 \== \-30dB))  
* Edit decision list: Input segments to play (with wet/dry mix) with what fade  
* Preview playback   
  * Button and hotkey control to play, pause, skip ahead back by word, paragraph  
  * No editing during playback  
* UI  
  * Text bubbles with transcript  
  * Non-speech sounds are given their own bubbles (speaker “\[None\]”) with a description of the sound in brackets, defaulting to “\[Sound\]” if sound is not identified.  
    * Sweep through non-speech segments in 20ms windows (overlapping by 10ms) to look for RMS \> 4 \* room tone RMS, with 150 ms “hold time” (continue searching forward for additional sound to treat as the same event as long as sound reoccurs within the hold time)  
  * Color according to speaker  
  * Vertical spacing according to timeline  
  * Multiple columns for over-speaking  
  * Current location cursor   
  * Range selection (for delete/undelete)   
  * Export button for each track, button for rendered export  
  * Menu options  
  * Close/open/new project  
  * Import dialog: single vs. multiple tracks, append vs prepend vs align  
  * Remove track  
  * Process files in the background with a progress bar indicator  
    * Note: Background queue needs to be able to accept multiple types of requests and multiple requests.   
    * Show current step/request next to the progress bar  
    * Progress bar is progress for the current step  
    * Show spinner when there isn’t a progress bar  
    * Show step queue count (e.g., 3/12) along with step/request name  
  * Notifications (where? Progress bar?)  
  * Settings: Grouped into tabs listed down left side of dialog  
* Note: Frontend can only trigger explicitly allowed backend events through the established API (no sending raw scripts or commands)  
* Accessibility  
  * All mouse actions have a keyboard or menu alternative  
  * Each word is its own span with tabindex="-1” so they can receive focus  
  * Mark cursor with aria-current="location"  
  * When requested, announce location with aria-live="polite"  
  * Bubbles are \<section\> with aria-labelledby to link to bubble label  
* Models  
  * Bundled: Whisper Tiny, Pyannote (diarization), Gemma something small  
  * Dialog to upgrade to bigger Whisper/Gemma and add Wav2vec2, MP-SENet  
  * Probably Gemma 3 12B Q4\_K\_M for more-capable systems (at least 16GB RAM), falling back to Gemma 3 4B Q8 or Q4 (for 8-16 GB RAM). Gemma 3 1B for “GPU-poor” systems (integrated graphics/CPU-only).  
  * Silero VAD instead of Pyannote for resource-poor systems?  
  * YAMnet for non-speech sound classification  
  * Note: Record which models were used for processing in the project data structure  
* Data structures  
  * Time is represented in integer samples using the project sampling rate  
  * Transcript is composed of a list of track timelines, each represented by a tree of turns, turn nodes are a linear array of words  
  * After import, non-speech indicators (“\[Sound\]”, etc.) behave as transcribed words.  
  * Each node includes its turn duration, its post-turn silence duration, and the sum of left subtree durations; list of words, list of audio splices  
  * Words include: type (normal, disfluency, sound), start and end timestamps (float) in the underlying audio file, text label, mute and cut boolean states, index of audio splice word belongs to  
  * Audio splice: start time and length in the project timeline (integer samples in project sample rate), and one of: (a) audio source start index for decoding (integer in source file sample rate, including frame alignment and resample padding), offset (i.e., how many resampled samples to throw out), (b) room tone indicator, (c) silence indicator  
  * Tree is a Augmented Relative Event duration Tree, specifically  
    * Arena-allocated augmented interval tree  
    * On an index-backed/Arena tree (nodes are stored in a vec, pointers are just indices in the vec)  
  * Maintain a list of dead nodes, on create, first check whether a dead node can be reused  
  * On save snapshot, first, compact and reorder the backing array, constructing an index map along the way. The UI continues to use the compacted backing array (and updated track start nodes), while a background write process takes the old backing array and the remapping index to write out the compacted array state to the project database  
    * This only applies once we implement reordering in phase 2  
  * Labels are 0-length nodes in the tree, with a separate list of label node IDs  
* Documentation  
  * Feature reference manual  
  * Settings reference manual  
  * Internals reference manual (data structures, functions, backend/frontend API)

## Phase 2: Core Podcast Editing Features

* Timeline labels  
  * Label outline view  
  * Click label to jump to that point in transcript  
  * Button/hotkey to jump between labels  
  * If label is flagged as a section header, export with transcript  
  * Move label  
  * Label icon is small, text pops up on mouse hover or when cursor is at location.  
  * Label marker on track ribbon  
* Editing  
  * Split turn into two  
  * Rearrange turns  
  * Copy/cut paste (small crossfade at boundary, 50ms?)  
  * Manual adjustment of track alignment  
  * Fix transcription errors (includes inserting text for completely unrecognized speech; can this be force-aligned afterward?)  
  * Reassign speaker for given turn; merge speaker (all turns)  
  * Undo/redo  
  * Note: Letter-based shortcuts are case-insensitive  
  * Concatenate projects  
* Breath removal (reduce to 19-15dB)? Pop, saliva click removal  
  * De-clicker (what settings?)  
  * How to identify breaths?  
  * How to signal in the UI timeline?  
* Review/flip through identified filler words  
  * Disfluency removal only for selection, specific speaker, or specific track  
* Auxiliary audio  
  * Sounds effects and background music  
  * Distinguish between speech/aux at import  
  * Label aux audio tracks  
  * Insert at cursor  
* Preview playback  
  * Select tracks to mute during preview (show as striped ribbon)  
* Pacing  
  * Shorten silences (\>1.5s): Prompt:  
  * "Context: '\[Pre-text 10 words\] \<GAP\> \[Post-text 10 words\]'. Is this gap a (A) Sentence transition, (B) Mid-sentence hesitation, or (C) Content gap? Return the ideal duration in milliseconds.”  
  * Pacing style: Relaxed: LLM keeps longer pauses (800ms+). Snappy: LLM trims everything to "YouTube speed" (200ms-400ms). Radio: Standard professional timing.  
  * Granular pacing options (shorten silences over X seconds by Y percent)  
  * Add room tone: Identify longest gap in speech, loop with 100-200 ms linear crossfade (scale by dry signal weight if enhancing the track), 50 ms cross fade at the gap boundary  
  * Manual adjustment of gaps between speaker turns (cut/insert spacing vs. shift speech within existing gaps)  
  * Shorten intra-turn inter-word spaces by shrinking/expanding the turn’s bubble (distributes spaces evenly across the turn, with hard inter-word space minimum?)  
* Levels  
  * Adjust track levels, by bubble (with customizable linear fade length at beginning/end)  
  * Auto level: Loudness Normalization algorithm (-16 LUFS for stereo, \-19 for mono).  
  * Normalize to \-6dB: Automatically lower the volume of the whole file so the clipped peaks aren't hitting the ceiling anymore. Pass through VoiceFixer/MP-SENet: Let the AI "rebuild" the flat-topped waveforms. Apply a Limiter: Set a "Hard Limit" at \-1.0dB. This acts as a safety net to ensure that even after your AI "boosts" the audio, it never clips again.  
  * Cross-talk attenuation (how to prevent recognition of the same speech in multiple tracks?)  
  * Auto-mute track outside of designated speech turns  
  * Auto fade between sentences (downward expander): Threshold (typically between \-35dB and \-50dB), Ratio (1:2, or 1:3), Attach time (1-5ms), Release time (150ms to 300ms). Or is this based on timestamps?  
  * Manually adjust track fade  
  * Set left/right fade for each track  
* EQ  
  * Manual control (does it need a spectral view?)  
  * FlowEQ? Or DeepAFx-ST or Automatic Audio Equalization with Semantic Embeddings framework for blind spectrogram inversion  
  * Alternatively, AutoEQ with a few target samples built in  
* I/O  
  * Export stereo or mono tracks (according to settings)  
* Persistent voice identification labels  
* Restore queued actions  
  * Store the background processing queue in the sqlite database, popping when command is finished  
  * On open, if there are items in the stored queue, display dialog to user asking whether to restart them (with checkboxes for which to restart); otherwise, clear the queue in the database.  
* UI  
  * Transcript search (including/not including deleted words)  
  * Track ribbon (for making track-wise adjustments)  
  * Open recent project list  
  * Undo  
  * Project file compaction (reinitialize from current state)  
  * End of timeline handle to crop/insert silence after the last turn  
* Export  
  * Export all tracks to individual files  
  * Export mixed subset of tracks (based on which tracks are muted)  
  * Export selection (single track, individual tracks, or mixed), padded  
  * Option to name based on metadata  
* Options/settings  
  * Mute to silence, rather than room tone  
* Models  
  * Advanced option to point to pre-downloaded weights  
  * Option to select between models  
  * OpenVino for Intel CPU?  
* Documentation  
  * Quick start guide  
  * Podcast tutorial

## Phase 3: Core Podcast Recording Features

* The ability to easily assign different USB mics or audio interfaces to separate tracks without needing complex third-party routing software.  
* Record second track at \-12 dB in case of clipping  
* A dedicated panel to trigger intros, outros, ad reads, and sound effects live during recording, saving hours in post-production.  
* Insert labels/bookmarks during recording  
* Notes pane

## Phase 4: Core Features for Additional Use Cases

* Script view  
  * Word, PDF, Markdown, ePub  
  * Annotations  
* Teleprompter during recording  
* Text correction (in-painting)  
  * Shows in-filled words underlined  
* Script comparison: Highlight deviations  
  * Auto in-painting?  
* ACX standards?  
  * RMS Loudness (Must be between \-23dB and \-18dB)  
  * Peak Levels (No higher than \-3dB)  
  * Noise Floor (Below \-60dB)  
  * Room tone pad (1-5s at beginning and end)  
* Video preview for a single video track  
* Punch and roll: When recording, move cursor back to delete. Then preview previous 5 sec before continuing to record.  
* Auto punch and roll when using teleprompter  
  * Or just a flag for podcaster  
* Audiobook metadata management  
* Season-wise (book-wise) export all episodes (chapters)  
* Reducing friction for audio drama: file organization, tedious manual volume adjustments (smart ducking, e.g. \-6 dB), and managing overlapping tracks  
* Group tracks into speech, background, effects; adjust volume of all tracks in group  
* Visual pin for sound effects? Or split the speech bubble?  
  * Insert effect at cursor location within a bubble  
  * Or drag and drop onto a given word  
* Cropping of background sounds in the track ribbon  
  * Auto repeat to extend?  
* Find track in timeline by name  
* Documentation  
  * Audiobook user guide

## Phase 5: Additional Podcast Production Features

* Chapter detection, labeling, table of contents  
* Show notes  
* ID3 tags  
* Search through old transcripts  
* Episode/season planner  
* File archive manager to compress files, move to cloud storage  
* Scratchpad bin (A sidebar where editors can pull text snippets, quotes, or soundbites from various interview transcripts and arrange them into a storyboard before actually touching the main timeline.)  
  * Import folder, allow organization, preview  
  * Classify as speech, background, effects  
  * Drag and drop to timeline  
  * Record to scratchpad (note: can be used for character samples for audiobooks)  
* Smart music ducking (lower volume when speaking)  
* Documentation  
  * Podcast management user guide

## Phase 6: Advanced Features

* Hotkey customization  
* Have two projects open at once; permit copying between projects  
* Remote recording interface  
* Record video from webcam  
* VoiceFixer for heavily degraded audio  
* Overlapping speech separation  
* Video track (key frame for each speech bubble)  
* Video auto alignment based on audio correlation with main audio  
* Video active speaker detection  
* J-cuts, L-cuts  
* Social video clipper (suggest “killer” quotes, or let user select)  
* Integrate with podcast distribution platforms  
* Fine tune filler word removal (edges, crossfade, mute rather than cut)  
* Manually adjust space between words  
* Trackwise waveform view  
* MIDI foot pedals to start/stop recording  
* Vocal Health Analyzer (vocal clarity, mouth dryness (detecting excessive clicks), and background noise floor)  
* LLM labeling of sound without speech  
* Decorative formatting for transcript (bold, italics, underline)  
* Customize color assignment to speakers  
* Reverb presets (e.g., Small Bedroom, Large Cave, Tiled Bathroom, Empty Warehouse, or Inside a Car.)  
  * Background tint for ambient zones  
  * Automatic cross fade between zones  
* Search for/relink moved audio files  
* Smart create new speaker: After assigning several turns, re-run speaker assignment based on new speaker average embeddings.  
* Overtalk separator (from the same track?)  
* Auto check for app updates  
* Resynthesize speech at faster/slower pace  
* Identify a list of technical terms, proper names, etc. that are high candidates for manual review for accuracy in the transcript  
* Scripting hooks  
  * Python or JavaScript?  
  * Plugin system  
* Documentation  
  * Granular control tutorial

## Partial Tech Stack

* Tauri with native styling, Svelt, Bits UI, Tailwind CSS  
* Python sidecar with CPU-only PyTorch (download GPU PuTorch optionally at model selection if GPU detected via torchruntime)  
  * Compile with nuitka? Note that torch and WhisperX would need to be loaded by importlib, instead of standard import syntax.  
* WhisperX  
* Gemma (which?)  
* MP-SENet (reconstruction)  
* F5-TTS (in-painting)  
* Hugo to coordinate documentation

## Phase 1 User Experiences

* Initial run: Model download dialog  
  * “Welcome to Vocalboard\!” \[additional explanation\]  
  * Select preview (no download), recommended (models selected based on detected system capabilities), or custom (choose from radio buttons for each model component)  
  * Download and disk sizes listed next to model options  
  * Preview option has label indicating models can be downloaded/changed later in the settings  
  * Checkbox to download multiple models (auto selected custom, changes radio buttons to check boxes)  
  * Button to use separately downloaded weights, opens appropriate settings dialog  
  * If models are selected for download, they are queued in the background with a progress bar in the progress bar area (progress bar has a label for what type of progress is being monitored)  
  * On download complete, if no model is already selected for the given component, it is automatically selected  
* Model settings  
  * For each model component, list of available models, with selected model highlighted, those not yet downloaded in grey with double click/right click to queue a download, right click to remove model  
  * (Note: it is possible to have no model for a given component)   
  * Option to select a model from an external location  
  * Button to select recommended models for detected system specs (queues download, as necessary, after confirmation of download size)  
* Program launch: Welcome screen  
  * Logo/title banner with buttons for: New project, Open project, Recent projects  
* Open project  
  * Open dialog  
  * Check that all audio files in the project exist. If missing an enhanced track, warn user and ask if enhancement should be recalculated. If missing a main track, ask is user wants to remove, or locate the file, or ignore. Tracks that could not be loaded are silently ignored in playback/export.  
  * Load state snapshot from project database; play forward any commands in the log after the snapshot point  
  * Main UI shown: Speech bubbles with track ribbons on left, ribbons ordered from first track to initiate, each bubble titled with speaker name and starting timestamp on the project timeline, color ribbons and bubbles according to speaker, vertical size of bubbles and distance between bubbles governed by the length of the turn  
  * Cursor is set to the first word in the timeline, or to the track start locus of the first track if the transcript returned no words  
* New project  
  * Save dialog to get project filename  
  * If not canceled, initialize vocalboard sqlite database in chosen location  
  * Show blank timeline UI  
  * Automatically start add speech track flow (can be canceled)  
* Add speech track  
  * Button/menu/hotkey triggers open dialog  
  * User selects one or more speech audio tracks (if more than one, they are aligned together)  
  * Track is run through WisperX with speaker diarization, followed by alignment, if more than one track selected, followed by room tone detection, followed by non-speech sound detection  
  * If this is the project’s first speech track, popup progress bar while initial processing is happening; otherwise, tracks are processed in background (with progress bar) and appended to project timeline.  
  * Speakers merged with existing speakers if cuisine similarity \< 0.71 (or 0.5?)  
  * Speakers are initially named “Speaker \#” ordered consecutively.  
  * Project transcript UI updated when complete  
* Track info  
  * Triggered from menu, hotkey, or double clicking track ribbon  
  * Informational dialog with title, audio file location, codec and sampling rate, project timeline start, length, and models used for processing  
  * Also include project info: Project sampling rate, total duration  
  * Also include current turn info: Duration in project, original audio duration, post-turn silence duration  
* Rename track (analogously for speaker)  
  * At import, tracks are initially named by filename; deduplicate track names by numeric suffix  
  * When rename track menu item is selected, popup dialog with text box with current track name  
  * If user clicks Cancel, do nothing  
  * If user clicks OK and track name is not empty and track name is not equal to any other track name, update name of track  
  * If user tries to enter an empty track name display “Track name may not be empty” and return to track rename dialog with current track name  
  * If user tries to enter an existing track name, display “Track \[name\] already exists”  and return to track rename dialog with the name the user tried to enter  
  * Track name is displayed by tool tip when cursor is at track start locus and when user hovers mouse over  
* Remove track  
  * If there is only one track, display dialog indicating that the last track cannot be removed.  
  * Otherwise, display confirmation dialog  
  * If user clicks OK, proceed with removal  
  * Update UI  
  * Cursor is set to the next word on the project timeline (or the previous word if no later words, or the previous start track location if transcript has no more words)  
* Timeline navigation  
  * App always defaults focus to the transcript/timeline when a dialog isn't being shown  
  * A cursor tracks the current word/element, highlighting it with a background color.  
  * Bubble(s) containing the current cursor or part of the selected range have a heavier border.  
  * Click on word sets cursor, click in bubble not on word sets cursor at first word in turn  
  * Scroll bars  
  * Click and drag or shift+click adjusts selection  
  * Keyboard navigation (see hotkey list)  
  * The start of a track is a distinct location for cursor focus (but it is never included in selection ranges). If the first turn begins at exactly track time 0, the start of track cursor locus occurs prior to the first turn locus. The order of tracks that start at exactly the same project time is arbitrary, but fixed (based on order tracks were imported).  
  * The start of track locus and the last word in each track are placed in an ordered queue by project timestamp. The Ctrl-Page up/down keys move to the previous/next location in this queue, relative to the current cursor location.  
  * Navigation includes cut/muted words (note: screen reader needs to announce “cut” or “muted” when reading these)  
  * Simultaneous speech (typically resulting from multiple tracks) is represented with bubbles in multiple columns. Note that a given turn may overlap with more than one other turn (e.g., speaker in track one starts speaking, speaker in track two starts speaking before the first turn is finished, speaker in first track finishes turn and starts a new turn before speaker in second track finishes speaking).  
  * Navigation across overlapping turns (bubbles) proceeds through each turn in its entirety before continuing at the start of the next overlapping turn, where overlapping turns are ordered by their start time. Note that this means that the cursor actually moves backward in the project timeline when advancing forward past the end of the earlier overlapping turn into the beginning of the later overlapping turn.  
* Playback preview  
  * Start playback from cursor (or selection range start, or current turn start)   
  * Editing commands are inactive during playback  
  * Stop playback at end of selection range/current turn, or end of last track, or spacebar  
  * Audio is played according to project timeline, mixing aligned tracks, as necessary, substituting room tone for muted words, and skipping cut words  
  * As preview plays, the current transcript word is highlighted (note that more than one transcript word may be highlighted at once if there is overlapping speech)  
  * When playback stops, cursor location is set to last word played (selection is cleared)   
  * Note that screen reader should not announce words as they are highlighted during playback, should announce section and word of new cursor location when playback ends  
* Align tracks  
  * Menu option triggers dialog window with list of tracks by name and checkboxes (tracks that have already been aligned are grouped together with the same checkbox), ordered in current project timeline order  
  * Track of current cursor location has checkbox selected by default  
  * User selects checkboxes for tracks to align.   
  * If user clicks cancel or the user clicks okay with fewer than 2 checkboxes checked, dialog is dismissed with no further action.   
  * If user clicks okay with at least two checkboxes checked, an alignment request starts processing in the background with progress bar.   
  * When alignment is complete, UI is updated with new project timeline.  
* Cut/uncut, mute/unmute  
  * May be applied to the word at the cursor or to the entire selection (if selection is a range)   
  * UI cut/mute action toggle between cut/uncut or mute/unmute.  
  * Cut: If entire selection is cut, set to uncut, otherwise set to cut.  
  * Mute: If entire selection under consideration is mute, set to unmute. Otherwise, set to mute.  
  * Note that setting mute on text that is set as cut has no immediate practical consequence, but it affects what happens if the text is later uncut (i.e., it will remain muted, even though the previously cut time is restored to the timeline). Similarly unmuting cut text has no immediate effect until/unless the text is also uncut.  
  * When cutting/uncutting, timestamps in UI are updated. Cutting a word includes cutting the inter-word silence after the cut word, if the given turn has additional words that follow.  
  * Note that cutting from one track results in a cut (of silence) in all other overlapping tracks.  
  * A word may not be cut if it is overlapping with a turn from another track (words from a turn that overlaps another turn may be cut if the word in question is not part of the overlap). If user tries to cut an overlapping word, show warning, then cancel action.  
  * Exception: If selection range includes the full overlapping turns, they may be cut together. Uncut of an overlapping turn requires a selection covering the full overlapping turns.  
  * Cut text is displayed as grey with strike through.   
  * Mute text is displayed as grey in square brackets—note that consecutive mute words in the same turn (i.e., bubble) are bracketed together as a range.  
  * Sound during muted text is replaced with room tone  
* Enhance audio (clean up audio)  
  * If current track has not been enhanced, check that enhancement model is present. If not, display message asking if user wants to download model now. If yes, show model selection dialog. Queue download of the models with a progress bar. Otherwise, return to project.  
  * Then, open Save dialog to get path for enhanced track, default to track file path with “-enhanced” added to filename.  
  * Then, queue the enhancement process in the background with progress bar, saving enhanced audio as FLAC on disk  
  * When enhancement is complete (or if enhancement was previously calculated), open dialog with track name and slider from 0 to 100 for wet/dry mix with “preview/pause” button (plays just the given track from the next word in the track from the current cursor location, or from the first word in the track if no more words in track after current cursor)   
  * Default value is 50% (suggest a smart value between 20-80% based on data?)  
* Disfluency removal (clean up speech)  
  * Applies to current track, or current selection (if there is a selection range)  
  * When selected from menu, check whether disfluencies have been identified for this/all tracks (need a flag in the track data model)   
  * For tracks for which disfluencies have not yet been identified, start a background task with progress bar for LLM to identify them   
  * When disfluency list is ready, update data model to tag words.   
  * Then, pop up to notify user that disfluencies are ready for removal and ask whether to proceed. \[Skip this if disfluency list was already computed\]  
  * If user confirms, run cut on all disfluencies (treat as a single command in undo stack), with progress bar. Note: no user-initiated editing while applying the cuts. UI should remain responsive for viewing.  
  * Otherwise, do nothing more.   
  * If disfluencies were previously identified for this/all tracks, process directly to applying the cuts.  
  * Give informational dialog when cuts are complete.  
  * Note: A user may uncut disfluencies and then re-run disfluency removal to re-cut, so the command should run through the disfluency list even if the command was run previously.  
* Sound removal  
  * For current track or current selection, Run cut on all sounds (treat as single command in undo stack), with progress bar. Note: no user-initiated editing while applying these cuts. UI should remain responsive for viewing.  
  * Give informational dialog when cuts are complete.  
* Export track/mixed/transcript  
  * Save dialog to get file path and type (default directory is project directory if saved, track directory if not; default name is track/project name; default type is FLAC/vtt)  
  * Identify format based on file type  
  * If unsupported file type, present error message and return to save dialog with the same name previously submitted  
  * Track is padded with silence to project length  
* Toolbar buttons  
  * New, Open  
  * Preview (play)  
  * Next/previous by speaker  
  * Add speech track  
  * Clean audio  
  * Clean speech  
* Menus  
  * File  
    * New project  
    * Open project  
    * Recent projects   
    * Save project as  
    * Export track  
    * Export mixed  
    * Export transcript  
    * Settings  
    * Quit  
  * Edit  
    * Cut/uncut  
    * Mute/unmute  
    * Clean up speech  
    * Remove noises  
    * Select all  
  * Track  
    * Add speech track  
    * Track info  
    * Align tracks   
    * Clean up audio  
    * Rename track  
    * Remove track  
    * Rename speaker (not enabled if cursor is at track start locus)  
    * Cancel current background task  
  * Help  
    * Shortcut list  
    * Documentation (open browser rendering local copy of documentation)   
    * About  
  * Track context menu  
    * (Right click on track ribbon, or activate context menu from track start locus)  
    * Track info  
    * Align track (auto check checkbox for this track in the align dialog)   
    * Clean up audio  
    * Clean up speech  
    * Rename track  
    * Export track  
    * Remove track  
  * Speech bubble context menu  
    * Rename speaker  
    * Cut/uncut (select entire turn, then apply cut)  
    * Mute/unmute (select entire turn, then apply mute)  
    * Preview audio  
    * \[Track context menu items\]  
  * Selection range context menu  
    * (If selection range is more than one word)  
    * Cut/uncut  
    * Mute/unmute  
    * Clean up speech  
    * Remove noises  
  * Notification/progress bar context window  
    * Cancel  
  * Settings  
    * Models \[see above\]  
    * Advanced  
      * \[Note: These are locked in at project creation.\]  
      * Project sample rate (default: 48 kHz)  
      * Rubato sinc interpolation parameters  
      * \[Other thresholds/constants used in processing algorithms\]

## Shortcut/Hotkey List

* Navigation  
  * Left/right arrow: Move cursor by word  
  * Up/down arrow: Move cursor to start of previous/next turn  
  * Ctrl-Left: Move cursor to start of current turn  
  * Page up/down: Move cursor to previous/next label   
  * Ctrl-Page up/down: Move cursor up/down by scroll page  
  * Home/end: Move cursor to start/end of project timeline   
  * Ctrl-home/end: Move cursor to start/end of the track containing the element at the cursor  
  * Open/close square bracket: Move to previous/next turn by same speaker  
  * Shift modifying any of the above: Adjust selection by corresponding cursor jump  
  * Ctrl-G: Modal dialog to accept a time, jump to that time, (checkbox to expand current selection to that time)  
  * Ctrl-L: Have screen reader announce location  
* Playback/Recording  
  * Space: Start/pause playback  
  * Ctrl-Space: Play current speech turn  
* Editing  
  * Delete: Cut/uncut current selection (or word at cursor)  
  * Ctrl-Delete: Mute current selection (or word at cursor)  
  * Ctrl-Up/down: Swap current turn with previous next turn  
  * Alt-Up/down: Increase/decrease space between turned  
  * Alt-Left/right: Increase/decrease space between words  
  * Ctrl-A: Select all  
  * Ctrl-X: Cut current selection  
  * Ctrl-V: Paste timeline section  
  * Ctrl-C: Copy timeline section  
* Global  
  * Ctrl-S: Save project as  
  * Ctrl-O: Open project  
  * Ctrl-N: New project  
  * Ctrl-Q: Quit  
  * Ctrl-H: Bring up shortcut list  
  * Ctrl-I: Track info
