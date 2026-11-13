use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::audio::{is_supported_extension, probe_track};
use crate::cli::validate_add_path;
use crate::error::{Result, TunrError};
use crate::ipc::TargetKind;
use crate::paths::{lock_path_for, with_file_lock, write_json_atomic};
use crate::platform::canonicalize_clean;

pub const LIBRARY_VERSION: u32 = 1;
const INVALID_ONE_BASED_INDEX_MSG: &str = "index must be >= 1 (1-based from `tunr list`)";

/// Metadata record for a single audio track indexed in `library.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub path: PathBuf,
    pub duration_secs: Option<f64>,
}

/// Named ordered list of song IDs referencing entries in `Library::songs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub name: String,
    pub song_ids: Vec<String>,
}

/// Persistent index-only music library store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Library {
    pub version: u32,
    pub songs: Vec<Song>,
    pub playlists: Vec<Playlist>,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            version: LIBRARY_VERSION,
            songs: Vec::new(),
            playlists: Vec::new(),
        }
    }
}

/// Summary returned by `Library::add_files`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddSummary {
    pub added_count: usize,
    pub existing_count: usize,
    pub added_titles: Vec<String>,
    pub playlist_name: Option<String>,
    pub playlist_added_count: usize,
    pub skipped_files: Vec<String>,
}

/// Summary returned by `Library::update_workspace`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateSummary {
    pub added_count: usize,
    pub relinked_count: usize,
    pub removed_count: usize,
    pub unchanged_count: usize,
    pub skipped_files: Vec<String>,
}

/// Outcome returned by `Library::delete_target`.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteOutcome {
    Song(Song),
    Playlist(Playlist),
    PlaylistSong {
        playlist: Playlist,
        song: Song,
        index: usize,
    },
}

/// Disambiguation candidate describing a matching song entry in [`Library::songs`].
#[derive(Debug, Clone, PartialEq)]
pub struct SongCandidate {
    pub index: usize,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub path: PathBuf,
    pub duration_secs: Option<f64>,
}

impl SongCandidate {
    fn from_song(index: usize, song: &Song) -> Self {
        Self {
            index,
            title: song.title.clone(),
            artist: song.artist.clone(),
            album: song.album.clone(),
            path: song.path.clone(),
            duration_secs: song.duration_secs,
        }
    }
}

/// Result of resolving a `play` target query with optional artist disambiguation.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayResolution {
    Ready(PlayTarget),
    AmbiguousSongs {
        query: String,
        candidates: Vec<SongCandidate>,
    },
}

/// Resolved playback target and ordered queue ready for `IpcRequest::Play`.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    Playlist {
        playlist: Playlist,
        songs: Vec<Song>,
        start_index: Option<usize>,
    },
    Song {
        selected: Song,
        queue: Vec<Song>,
    },
}

impl PlayTarget {
    /// Convert into `(queue, start_index, playlist_name)` for `IpcRequest::Play`.
    pub fn into_play_parts(self) -> (Vec<Song>, Option<usize>, Option<String>) {
        match self {
            PlayTarget::Playlist {
                playlist,
                songs,
                start_index,
            } => (songs, start_index, Some(playlist.name)),
            PlayTarget::Song { queue, .. } => (queue, Some(0), None),
        }
    }
}

enum LookupMatch {
    None,
    One(usize),
    Multiple(Vec<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SongLookupMode {
    Strict,
    Flexible,
}

impl Library {
    fn load_unlocked_with_status(path: &Path) -> Result<(Self, bool, bool)> {
        if !path.exists() {
            return Ok((Self::default(), false, false));
        }
        let raw = fs::read_to_string(path)?;
        let mut lib: Library = serde_json::from_str(&raw)?;
        let normalized = lib.normalize_existing_songs();
        Ok((lib, true, normalized))
    }

    fn load_unlocked(path: &Path) -> Result<Self> {
        Self::load_unlocked_with_status(path).map(|(lib, _, _)| lib)
    }

    /// Normalize legacy `"Artist - Title"` entries in place; returns `true` if any song changed.
    pub(crate) fn normalize_existing_songs(&mut self) -> bool {
        let mut changed = false;
        for song in &mut self.songs {
            let (norm_title, norm_artist) =
                normalize_track_metadata(Some(&song.title), song.artist.as_deref(), &song.path);
            if song.title != norm_title || song.artist != norm_artist {
                song.title = norm_title;
                song.artist = norm_artist;
                changed = true;
            }
        }
        changed
    }

    /// Load library from `path` under a shared advisory lock, or return an empty default `Library` if absent.
    pub fn load(path: &Path) -> Result<Self> {
        with_file_lock(&lock_path_for(path), false, || Self::load_unlocked(path))
    }

    /// Persist library atomically via a unique sibling temporary file followed by rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }

    /// Atomically update `path` under an exclusive advisory lock (`load` -> `mutator` -> atomic save).
    pub fn update<T>(
        path: &Path,
        mutator: impl FnOnce(&mut Library) -> Result<T>,
    ) -> Result<T> {
        with_file_lock(&lock_path_for(path), true, || {
            let mut lib = Self::load_unlocked(path)?;
            let out = mutator(&mut lib)?;
            write_json_atomic(path, &lib)?;
            Ok(out)
        })
    }

    /// Atomically update `path` under an exclusive advisory lock, skipping the disk write
    /// when `path` already existed and neither normalization nor `mutator` changed the library.
    pub fn update_if_changed<T>(
        path: &Path,
        mutator: impl FnOnce(&mut Library) -> Result<T>,
    ) -> Result<T> {
        with_file_lock(&lock_path_for(path), true, || {
            let (mut lib, existed, normalized) = Self::load_unlocked_with_status(path)?;
            let before = if existed && !normalized {
                Some(lib.clone())
            } else {
                None
            };
            let out = mutator(&mut lib)?;
            if !existed || normalized || before.as_ref() != Some(&lib) {
                write_json_atomic(path, &lib)?;
            }
            Ok(out)
        })
    }

    /// Remove all songs whose IDs appear in `ids` and strip those IDs from every playlist.
    fn remove_songs_by_ids(&mut self, ids: &HashSet<String>) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let before = self.songs.len();
        self.songs.retain(|song| !ids.contains(&song.id));
        let removed = before.saturating_sub(self.songs.len());
        for pl in &mut self.playlists {
            pl.song_ids.retain(|id| !ids.contains(id));
        }
        removed
    }

    /// Reconcile the workspace (and optional separate `download_dest`) against `library_path`:
    /// adds newly discovered audio files, relinks uniquely matched moved/renamed workspace files
    /// while preserving `Song::id` and playlist membership, and prunes deleted workspace files.
    pub fn update_workspace(
        library_path: &Path,
        working_dir: &Path,
        config_dir: &Path,
        playlists_dir: &Path,
        download_dest: Option<&Path>,
    ) -> Result<UpdateSummary> {
        let working_dir = canonicalize_clean(working_dir)?;
        let config_dir =
            canonicalize_clean(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
        let playlists_dir =
            canonicalize_clean(playlists_dir).unwrap_or_else(|_| playlists_dir.to_path_buf());

        let mut scan_roots = vec![working_dir];
        let mut skipped_files = Vec::new();

        if let Some(dest) = download_dest {
            if dest.is_dir() {
                match canonicalize_clean(dest) {
                    Ok(dest_canon) => {
                        if !scan_roots.iter().any(|r| dest_canon.starts_with(r)) {
                            scan_roots.push(dest_canon);
                        }
                    }
                    Err(err) => skipped_files.push(format!(
                        "download destination {}: {err}",
                        dest.display()
                    )),
                }
            } else if dest.exists() {
                skipped_files.push(format!(
                    "download destination {} is not a directory; skipping scan",
                    dest.display()
                ));
            } else {
                skipped_files.push(format!(
                    "download destination {} does not exist (unmounted?); skipping scan",
                    dest.display()
                ));
            }
        }

        // Phase 1: scan and probe outside the exclusive library lock.
        let snapshot = Self::load(library_path)?;
        let snapshot_paths: HashSet<PathBuf> =
            snapshot.songs.iter().map(|s| s.path.clone()).collect();

        let skip_dirs = [config_dir, playlists_dir];
        let mut discovered_paths = Vec::new();
        let mut discovered_set = HashSet::new();

        for root in &scan_roots {
            let (files, walk_skipped) = walk_audio_files(root, &skip_dirs, &[".tunr-dl-tmp"])?;
            skipped_files.extend(walk_skipped);
            for path in files {
                if discovered_set.insert(path.clone()) {
                    discovered_paths.push(path);
                }
            }
        }
        discovered_paths.sort();

        let mut probed_candidates = Vec::new();
        for path in discovered_paths {
            if snapshot_paths.contains(&path) {
                continue;
            }
            match probe_song(path.clone()) {
                Ok(song) => probed_candidates.push(song),
                Err(err) => skipped_files.push(format!("{}: {err}", path.display())),
            }
        }

        // Phase 2: reconcile atomically under exclusive lock, writing only if changed.
        Self::update_if_changed(library_path, |lib| {
            let existing_paths: HashSet<PathBuf> =
                lib.songs.iter().map(|s| s.path.clone()).collect();
            let mut candidates: Vec<Song> = probed_candidates
                .into_iter()
                .filter(|c| !existing_paths.contains(&c.path))
                .collect();

            let mut missing_indices: Vec<usize> = lib
                .songs
                .iter()
                .enumerate()
                .filter(|(_, song)| {
                    scan_roots.iter().any(|root| song.path.starts_with(root))
                        && !discovered_set.contains(&song.path)
                        && !song.path.exists()
                })
                .map(|(idx, _)| idx)
                .collect();

            let mut relinked_count = 0usize;

            // Pass 1: unique match by (title, artist, album, duration_bucket) with valid duration.
            let (pass1_matches, remaining_missing, remaining_cand_indices) =
                match_unique_moves(lib, &missing_indices, &candidates, |song| {
                    let bucket = duration_bucket(song.duration_secs)?;
                    Some((
                        song.title.clone(),
                        song.artist.clone(),
                        song.album.clone(),
                        bucket,
                    ))
                });
            for (song_idx, cand_idx) in pass1_matches {
                apply_relink(&mut lib.songs[song_idx], &candidates[cand_idx]);
                relinked_count += 1;
            }
            missing_indices = remaining_missing;
            candidates = remaining_cand_indices
                .into_iter()
                .map(|idx| candidates[idx].clone())
                .collect();

            // Pass 2: unique match by (file_name, duration_bucket).
            let (pass2_matches, remaining_missing, remaining_cand_indices) =
                match_unique_moves(lib, &missing_indices, &candidates, |song| {
                    let file_name = song.path.file_name().map(|n| n.to_os_string());
                    Some((file_name, duration_bucket(song.duration_secs)))
                });
            for (song_idx, cand_idx) in pass2_matches {
                apply_relink(&mut lib.songs[song_idx], &candidates[cand_idx]);
                relinked_count += 1;
            }

            let unmatched_candidates: Vec<Song> = remaining_cand_indices
                .into_iter()
                .map(|idx| candidates[idx].clone())
                .collect();
            let added_count = unmatched_candidates.len();
            lib.songs.extend(unmatched_candidates);

            let remove_ids: HashSet<String> = remaining_missing
                .into_iter()
                .map(|idx| lib.songs[idx].id.clone())
                .collect();
            let removed_count = lib.remove_songs_by_ids(&remove_ids);

            let unchanged_count = lib
                .songs
                .len()
                .saturating_sub(added_count + relinked_count);

            Ok(UpdateSummary {
                added_count,
                relinked_count,
                removed_count,
                unchanged_count,
                skipped_files,
            })
        })
    }

    /// Find a song reference by its unique UUID string.
    pub fn find_song_by_id(&self, id: &str) -> Option<&Song> {
        self.songs.iter().find(|s| s.id == id)
    }

    /// Resolve all existing songs referenced by `playlist` in playlist order.
    pub fn songs_for_playlist(&self, playlist: &Playlist) -> Vec<Song> {
        playlist
            .song_ids
            .iter()
            .filter_map(|id| self.find_song_by_id(id).cloned())
            .collect()
    }

    /// Resolve `(raw_song_ids_pos, Song)` entries for a playlist, preserving 1-based list order
    /// even when dangling IDs exist in `song_ids`.
    fn resolve_playable_playlist_entries(&self, pl_idx: usize) -> Vec<(usize, Song)> {
        self.playlists[pl_idx]
            .song_ids
            .iter()
            .enumerate()
            .filter_map(|(pos, id)| self.find_song_by_id(id).cloned().map(|song| (pos, song)))
            .collect()
    }

    /// Append non-duplicate `song_ids` to `playlist`, returning `(canonical_playlist_name, added_count)`.
    fn append_unique_song_ids(
        playlist: &mut Playlist,
        song_ids: &[String],
    ) -> (String, usize) {
        let mut added = 0;
        for song_id in song_ids {
            if !playlist.song_ids.contains(song_id) {
                playlist.song_ids.push(song_id.clone());
                added += 1;
            }
        }
        (playlist.name.clone(), added)
    }

    /// Resolve a target playlist index by name (or 1-based list index when enabled),
    /// creating a new empty playlist when a non-numeric name does not exist yet.
    fn resolve_or_create_playlist_index(
        &mut self,
        playlist_name: &str,
        allow_numeric_index: bool,
    ) -> Result<usize> {
        if allow_numeric_index && playlist_name.bytes().all(|b| b.is_ascii_digit()) {
            match playlist_name.parse::<usize>() {
                Ok(n) if n >= 1 && n <= self.playlists.len() => return Ok(n - 1),
                parsed => {
                    match self.lookup_playlist(playlist_name) {
                        LookupMatch::One(idx) => return Ok(idx),
                        LookupMatch::Multiple(names) => {
                            return Err(TunrError::Ambiguous(format!(
                                "playlist '{playlist_name}' matched multiple entries: {}",
                                names.join(", ")
                            )));
                        }
                        LookupMatch::None => {
                            return if matches!(parsed, Ok(0)) {
                                Err(TunrError::InvalidArg(
                                    INVALID_ONE_BASED_INDEX_MSG.to_string(),
                                ))
                            } else {
                                Err(index_out_of_bounds_error(
                                    "playlist",
                                    self.playlists.len(),
                                    playlist_name,
                                ))
                            };
                        }
                    }
                }
            }
        }

        match self.lookup_playlist(playlist_name) {
            LookupMatch::One(idx) => Ok(idx),
            LookupMatch::Multiple(names) => Err(TunrError::Ambiguous(format!(
                "playlist '{playlist_name}' matched multiple entries: {}",
                names.join(", ")
            ))),
            LookupMatch::None => {
                self.playlists.push(Playlist {
                    name: playlist_name.to_string(),
                    song_ids: Vec::new(),
                });
                Ok(self.playlists.len() - 1)
            }
        }
    }

    /// Append non-duplicate `song_ids` to an existing or newly created playlist, returning
    /// `(canonical_playlist_name, added_count)`.
    fn add_song_ids_to_playlist(
        &mut self,
        playlist_name: &str,
        song_ids: &[String],
    ) -> Result<(String, usize)> {
        let pl_idx = self.resolve_or_create_playlist_index(playlist_name, false)?;
        Ok(Self::append_unique_song_ids(&mut self.playlists[pl_idx], song_ids))
    }

    /// Resolve an existing library song query via `resolve_create_song_match`.
    fn resolve_existing_song_index(&self, trimmed: &str) -> Result<usize> {
        match self.resolve_create_song_match(trimmed)? {
            LookupMatch::One(idx) => Ok(idx),
            LookupMatch::None => Err(TunrError::NotFound(format!(
                "no song found matching '{trimmed}'"
            ))),
            LookupMatch::Multiple(matches) => Err(TunrError::Ambiguous(format!(
                "ambiguous song '{trimmed}': {} (use \"Title - Artist\" or list index)",
                matches.join(", ")
            ))),
        }
    }

    /// Add a file or recursive folder (`folder = true`) to the library index,
    /// optionally appending the added/existing songs to `playlist_name`.
    pub fn add_files(
        &mut self,
        path: &Path,
        folder: bool,
        playlist_name: Option<&str>,
    ) -> Result<AddSummary> {
        validate_add_path(path, folder)?;

        let clean_playlist = normalize_playlist_name(playlist_name)?;

        let (candidates, walk_skipped) = collect_candidate_files(path, folder)?;
        let mut summary = AddSummary {
            playlist_name: clean_playlist.clone(),
            skipped_files: walk_skipped,
            ..AddSummary::default()
        };
        let mut resolved_ids = Vec::with_capacity(candidates.len());
        let mut last_probe_err: Option<TunrError> = None;

        for canonical in candidates {
            if let Some(existing) = self.songs.iter().find(|s| s.path == canonical) {
                summary.existing_count += 1;
                resolved_ids.push(existing.id.clone());
                continue;
            }

            match probe_song(canonical.clone()) {
                Ok(song) => {
                    resolved_ids.push(song.id.clone());
                    summary.added_titles.push(song.title.clone());
                    summary.added_count += 1;
                    self.songs.push(song);
                }
                Err(err) => {
                    if !folder {
                        return Err(err);
                    }
                    summary
                        .skipped_files
                        .push(format!("{}: {err}", canonical.display()));
                    last_probe_err = Some(err);
                }
            }
        }

        if resolved_ids.is_empty()
            && let Some(err) = last_probe_err
        {
            return Err(err);
        }

        if let Some(pl_name) = clean_playlist {
            let (resolved_name, added) = self.add_song_ids_to_playlist(&pl_name, &resolved_ids)?;
            summary.playlist_name = Some(resolved_name);
            summary.playlist_added_count = added;
        }

        Ok(summary)
    }

    /// Add an audio file on disk (optionally to `playlist_name`), or add an existing library
    /// song matched by `query` into `playlist_name`.
    pub fn add_song(&mut self, query: &str, playlist_name: Option<&str>) -> Result<AddSummary> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(TunrError::InvalidArg(
                "song cannot be empty".to_string(),
            ));
        }

        let clean_playlist = normalize_playlist_name(playlist_name)?;
        let path = Path::new(trimmed);
        if path.exists() {
            return self.add_files(path, false, clean_playlist.as_deref());
        }

        if let Some(pl_name) = clean_playlist {
            let idx = self.resolve_existing_song_index(trimmed).map_err(|err| match err {
                TunrError::NotFound(_) => {
                    TunrError::NotFound(format!("song or audio file '{trimmed}' not found"))
                }
                other => other,
            })?;
            let song_id = self.songs[idx].id.clone();
            let (resolved_pl, playlist_added_count) =
                self.add_song_ids_to_playlist(&pl_name, &[song_id])?;
            Ok(AddSummary {
                added_count: 0,
                existing_count: 1,
                playlist_name: Some(resolved_pl),
                playlist_added_count,
                ..AddSummary::default()
            })
        } else {
            self.add_files(path, false, None)
        }
    }

    /// Add an existing library song (by title, display string, filename/stem, or 1-based index)
    /// to a playlist identified by name (creating it if absent) or 1-based playlist index.
    pub fn add_existing_song(
        &mut self,
        query: &str,
        playlist_target: &str,
    ) -> Result<AddSummary> {
        let trimmed_song = query.trim();
        if trimmed_song.is_empty() {
            return Err(TunrError::InvalidArg(
                "song cannot be empty".to_string(),
            ));
        }
        let clean_playlist = normalize_playlist_name(Some(playlist_target))?
            .expect("non-empty Some playlist yields Some");

        let song_idx = self.resolve_existing_song_index(trimmed_song)?;
        let song_id = self.songs[song_idx].id.clone();
        let pl_idx = self.resolve_or_create_playlist_index(&clean_playlist, true)?;
        let (resolved_pl, playlist_added_count) =
            Self::append_unique_song_ids(&mut self.playlists[pl_idx], &[song_id]);

        Ok(AddSummary {
            added_count: 0,
            existing_count: 1,
            playlist_name: Some(resolved_pl),
            playlist_added_count,
            ..AddSummary::default()
        })
    }

    /// Create a new playlist from existing song specifiers (titles, `"Title - Artist"` display
    /// strings, 1-based list indices, file names/stems, or unique title substrings).
    /// Fails atomically if the playlist already exists or any song entry is missing or ambiguous.
    pub fn create_playlist(&mut self, name: &str, song_titles: &[String]) -> Result<Playlist> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err(TunrError::InvalidArg(
                "playlist name cannot be empty".to_string(),
            ));
        }
        if song_titles.is_empty() {
            return Err(TunrError::InvalidArg(
                "playlist must contain at least one song".to_string(),
            ));
        }

        match self.lookup_playlist(trimmed_name) {
            LookupMatch::One(idx) => {
                return Err(TunrError::AlreadyExists(format!(
                    "playlist '{}' already exists",
                    self.playlists[idx].name
                )));
            }
            LookupMatch::Multiple(_) => {
                return Err(TunrError::AlreadyExists(format!(
                    "playlist '{trimmed_name}' already exists"
                )));
            }
            LookupMatch::None => {}
        }

        let mut resolved_ids = Vec::with_capacity(song_titles.len());
        let mut missing = Vec::new();
        let mut ambiguous = Vec::new();

        for raw_title in song_titles {
            let query = raw_title.trim();
            if query.is_empty() {
                continue;
            }
            match self.resolve_create_song_match(query)? {
                LookupMatch::One(idx) => {
                    let id = &self.songs[idx].id;
                    if !resolved_ids.contains(id) {
                        resolved_ids.push(id.clone());
                    }
                }
                LookupMatch::None => missing.push(query.to_string()),
                LookupMatch::Multiple(matches) => {
                    ambiguous.push(format!("{query} ({})", matches.join(", ")));
                }
            }
        }

        if !missing.is_empty() || !ambiguous.is_empty() {
            let mut parts = Vec::new();
            if !missing.is_empty() {
                parts.push(format!("missing song(s): {}", missing.join(", ")));
            }
            if !ambiguous.is_empty() {
                parts.push(format!("ambiguous song(s): {}", ambiguous.join("; ")));
            }
            let msg = parts.join("; ");
            if missing.is_empty() {
                return Err(TunrError::Ambiguous(msg));
            }
            return Err(TunrError::NotFound(msg));
        }

        if resolved_ids.is_empty() {
            return Err(TunrError::InvalidArg(
                "playlist must contain at least one non-empty song title".to_string(),
            ));
        }

        let playlist = Playlist {
            name: trimmed_name.to_string(),
            song_ids: resolved_ids,
        };
        self.playlists.push(playlist.clone());
        Ok(playlist)
    }

    /// Delete a song, playlist, or 1-based playlist entry (`<playlist>[<index>]`) from the
    /// library index WITHOUT deleting audio files from disk.
    /// Deleting a whole song strips its ID from every playlist; deleting a playlist entry keeps
    /// the song in `self.songs` and leaves empty playlists intact.
    pub fn delete_target(&mut self, query: &str, kind: TargetKind) -> Result<DeleteOutcome> {
        let q = query.trim();
        if q.is_empty() {
            return Err(TunrError::InvalidArg(
                "delete target cannot be empty".to_string(),
            ));
        }

        match kind {
            TargetKind::Song => self.delete_song_by_query(q),
            TargetKind::Playlist => {
                if !matches!(self.lookup_playlist(q), LookupMatch::None) {
                    return self.delete_playlist_by_query(q);
                }
                match parse_validated_bracket_index(q)? {
                    BracketIndexLookup::Target {
                        playlist_name,
                        one_based_index,
                    } => self.delete_playlist_song_by_index(playlist_name, one_based_index),
                    BracketIndexLookup::None => self.delete_playlist_by_query(q),
                }
            }
            TargetKind::Auto => {
                let song_match = self.lookup_song(q);
                let playlist_match = self.lookup_playlist(q);

                let has_song = !matches!(song_match, LookupMatch::None);
                let has_playlist = !matches!(playlist_match, LookupMatch::None);

                if has_song && has_playlist {
                    return Err(TunrError::Ambiguous(format!(
                        "'{q}' matches both a song and a playlist; specify `--song` or `--playlist`"
                    )));
                }
                if has_song {
                    return self.delete_song_by_query(q);
                }
                if has_playlist {
                    return self.delete_playlist_by_query(q);
                }
                match parse_validated_bracket_index(q)? {
                    BracketIndexLookup::Target {
                        playlist_name,
                        one_based_index,
                    } => self.delete_playlist_song_by_index(playlist_name, one_based_index),
                    BracketIndexLookup::None => Err(TunrError::NotFound(format!(
                        "no song or playlist found matching '{q}'"
                    ))),
                }
            }
        }
    }

    /// Resolve a song title or playlist name into a playable queue.
    /// When resolving a song, the library songs are rotated so the selected song starts at index 0
    /// and `next`/`prev` navigate through the rest of the library.
    pub fn resolve_play_target(&self, query: &str, kind: TargetKind) -> Result<PlayTarget> {
        match self.resolve_play_target_with_artist(query, kind, None)? {
            PlayResolution::Ready(target) => Ok(target),
            PlayResolution::AmbiguousSongs { query, candidates } => {
                let labels: Vec<String> = candidates
                    .iter()
                    .map(format_ambiguous_candidate)
                    .collect();
                Err(TunrError::Ambiguous(format!(
                    "multiple songs match '{query}': {}",
                    labels.join(", ")
                )))
            }
        }
    }

    /// Resolve a song or playlist target, returning [`PlayResolution::AmbiguousSongs`] when
    /// multiple songs match `query` (and optional `artist` filter) so interactive callers can
    /// prompt for disambiguation.
    pub fn resolve_play_target_with_artist(
        &self,
        query: &str,
        kind: TargetKind,
        artist: Option<&str>,
    ) -> Result<PlayResolution> {
        let q = query.trim();
        if q.is_empty() {
            return Err(TunrError::InvalidArg(
                "play target cannot be empty".to_string(),
            ));
        }
        let clean_artist = match artist {
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(TunrError::InvalidArg(
                        "artist filter cannot be empty".to_string(),
                    ));
                }
                Some(trimmed)
            }
            None => None,
        };

        if clean_artist.is_none() && q.bytes().all(|b| b.is_ascii_digit()) {
            return self.resolve_numeric_play_target(q, kind);
        }

        match kind {
            TargetKind::Song => self.resolve_song_play_candidates(q, clean_artist),
            TargetKind::Playlist => {
                if clean_artist.is_some() {
                    return Err(TunrError::InvalidArg(
                        "--artist cannot be used with --playlist".to_string(),
                    ));
                }
                if !matches!(self.lookup_playlist(q), LookupMatch::None) {
                    return self
                        .resolve_playlist_play_target(q)
                        .map(PlayResolution::Ready);
                }
                match parse_validated_bracket_index(q)? {
                    BracketIndexLookup::Target {
                        playlist_name,
                        one_based_index,
                    } => self
                        .resolve_playlist_play_target_at(playlist_name, Some(one_based_index))
                        .map(PlayResolution::Ready),
                    BracketIndexLookup::None => self
                        .resolve_playlist_play_target(q)
                        .map(PlayResolution::Ready),
                }
            }
            TargetKind::Auto => {
                if clean_artist.is_some() {
                    return self.resolve_song_play_candidates(q, clean_artist);
                }
                let has_strict_song = !self
                    .lookup_song_indices_with_tier(q, None, SongLookupMode::Strict)
                    .0
                    .is_empty();
                let playlist_match = self.lookup_playlist(q);
                let has_playlist = !matches!(playlist_match, LookupMatch::None);

                if !has_strict_song
                    && !has_playlist
                    && let BracketIndexLookup::Target {
                        playlist_name,
                        one_based_index,
                    } = parse_validated_bracket_index(q)?
                {
                    return self
                        .resolve_playlist_play_target_at(playlist_name, Some(one_based_index))
                        .map(PlayResolution::Ready);
                }

                let (song_indices, song_is_exact_title) =
                    self.lookup_song_indices_with_tier(q, None, SongLookupMode::Flexible);
                let has_song = !song_indices.is_empty();

                if has_song && has_playlist && song_is_exact_title {
                    return Err(TunrError::Ambiguous(format!(
                        "'{q}' matches both a song and a playlist; specify `--song` or `--playlist`"
                    )));
                }
                if has_playlist && !song_is_exact_title {
                    return self
                        .resolve_playlist_play_target(q)
                        .map(PlayResolution::Ready);
                }
                if has_song {
                    self.resolve_song_indices_to_resolution(q, None, song_indices)
                } else if has_playlist {
                    self.resolve_playlist_play_target(q)
                        .map(PlayResolution::Ready)
                } else {
                    Err(TunrError::NotFound(format!(
                        "no song or playlist found matching '{q}'"
                    )))
                }
            }
        }
    }

    /// Strictly resolve a 1-based list index (`tunr list`) into a playable song or playlist.
    pub fn resolve_play_index(
        &self,
        one_based_index: usize,
        kind: TargetKind,
        artist: Option<&str>,
    ) -> Result<PlayResolution> {
        if one_based_index == 0 {
            return Err(TunrError::InvalidArg(
                INVALID_ONE_BASED_INDEX_MSG.to_string(),
            ));
        }
        if artist.is_some() {
            return Err(TunrError::InvalidArg(
                "--artist cannot be used with --index".to_string(),
            ));
        }

        match kind {
            TargetKind::Playlist => self
                .play_target_for_playlist_index(one_based_index - 1)
                .map(PlayResolution::Ready),
            TargetKind::Song | TargetKind::Auto => {
                if one_based_index <= self.songs.len() {
                    self.play_target_for_song_index(one_based_index - 1)
                        .map(PlayResolution::Ready)
                } else {
                    Err(index_out_of_bounds_error(
                        "song",
                        self.songs.len(),
                        &one_based_index.to_string(),
                    ))
                }
            }
        }
    }

    /// Build a rotated [`PlayTarget::Song`] queue starting at `self.songs[index]`.
    pub fn play_target_for_song_index(&self, index: usize) -> Result<PlayTarget> {
        let selected = self
            .songs
            .get(index)
            .cloned()
            .ok_or_else(|| TunrError::NotFound(format!("song index {index} out of bounds")))?;
        let mut queue = Vec::with_capacity(self.songs.len());
        queue.extend_from_slice(&self.songs[index..]);
        queue.extend_from_slice(&self.songs[..index]);
        Ok(PlayTarget::Song { selected, queue })
    }

    /// Build a [`PlayTarget::Playlist`] for the playlist at 0-based `index` in `self.playlists`.
    pub fn play_target_for_playlist_index(&self, index: usize) -> Result<PlayTarget> {
        if index >= self.playlists.len() {
            return Err(index_out_of_bounds_error(
                "playlist",
                self.playlists.len(),
                &index.saturating_add(1).to_string(),
            ));
        }
        self.playlist_play_target_at(index, None)
    }

    fn delete_song_by_query(&mut self, query: &str) -> Result<DeleteOutcome> {
        let idx = self.resolve_unique_song_index(query)?;
        let song = self.songs.remove(idx);
        for pl in &mut self.playlists {
            pl.song_ids.retain(|id| id != &song.id);
        }
        Ok(DeleteOutcome::Song(song))
    }

    fn delete_playlist_by_query(&mut self, query: &str) -> Result<DeleteOutcome> {
        let idx = self.resolve_unique_playlist_index(query)?;
        let playlist = self.playlists.remove(idx);
        Ok(DeleteOutcome::Playlist(playlist))
    }

    fn delete_playlist_song_by_index(
        &mut self,
        playlist_query: &str,
        one_based_index: usize,
    ) -> Result<DeleteOutcome> {
        if one_based_index == 0 {
            return Err(TunrError::InvalidArg(
                INVALID_ONE_BASED_INDEX_MSG.to_string(),
            ));
        }
        let pl_idx = self.resolve_unique_playlist_index(playlist_query)?;
        let playable = self.resolve_playable_playlist_entries(pl_idx);
        let pl_name = &self.playlists[pl_idx].name;
        if playable.is_empty() {
            return Err(TunrError::NotFound(format!(
                "playlist '{pl_name}' contains no songs"
            )));
        }
        if one_based_index > playable.len() {
            return Err(TunrError::NotFound(format!(
                "playlist '{pl_name}' track index {one_based_index} out of bounds (playlist has {} track(s))",
                playable.len()
            )));
        }

        let (raw_pos, song) = playable
            .into_iter()
            .nth(one_based_index - 1)
            .expect("bounds checked above");
        self.playlists[pl_idx].song_ids.remove(raw_pos);
        Ok(DeleteOutcome::PlaylistSong {
            playlist: self.playlists[pl_idx].clone(),
            song,
            index: one_based_index,
        })
    }

    fn resolve_song_play_candidates(
        &self,
        query: &str,
        artist_filter: Option<&str>,
    ) -> Result<PlayResolution> {
        let indices = self.lookup_song_indices(query, artist_filter);
        self.resolve_song_indices_to_resolution(query, artist_filter, indices)
    }

    fn resolve_song_indices_to_resolution(
        &self,
        query: &str,
        artist_filter: Option<&str>,
        indices: Vec<usize>,
    ) -> Result<PlayResolution> {
        match indices.len() {
            0 => {
                let msg = match artist_filter {
                    Some(artist) => format!("no song found matching '{query}' by artist '{artist}'"),
                    None => format!("no song found matching '{query}'"),
                };
                Err(TunrError::NotFound(msg))
            }
            1 => Ok(PlayResolution::Ready(
                self.play_target_for_song_index(indices[0])?,
            )),
            _ => Ok(PlayResolution::AmbiguousSongs {
                query: query.to_string(),
                candidates: indices
                    .into_iter()
                    .map(|idx| SongCandidate::from_song(idx, &self.songs[idx]))
                    .collect(),
            }),
        }
    }

    fn resolve_create_song_match(&self, query: &str) -> Result<LookupMatch> {
        if query.bytes().all(|b| b.is_ascii_digit()) {
            return match query.parse::<usize>() {
                Ok(n) if n >= 1 && n <= self.songs.len() => Ok(LookupMatch::One(n - 1)),
                Ok(0) => {
                    let exact = self
                        .lookup_song_indices_with_tier(query, None, SongLookupMode::Strict)
                        .0;
                    if exact.is_empty() {
                        Err(TunrError::InvalidArg(
                            INVALID_ONE_BASED_INDEX_MSG.to_string(),
                        ))
                    } else {
                        Ok(self.indices_to_lookup_match(exact))
                    }
                }
                Ok(_) | Err(_) => {
                    let exact = self
                        .lookup_song_indices_with_tier(query, None, SongLookupMode::Strict)
                        .0;
                    Ok(self.indices_to_lookup_match(exact))
                }
            };
        }

        let matches = self
            .lookup_song_indices_with_tier(query, None, SongLookupMode::Flexible)
            .0;
        Ok(self.indices_to_lookup_match(matches))
    }

    fn lookup_song_indices(&self, query: &str, artist_filter: Option<&str>) -> Vec<usize> {
        self.lookup_song_indices_with_tier(query, artist_filter, SongLookupMode::Flexible)
            .0
    }

    fn lookup_song_indices_with_tier(
        &self,
        query: &str,
        artist_filter: Option<&str>,
        mode: SongLookupMode,
    ) -> (Vec<usize>, bool) {
        let q = query.trim();
        if q.is_empty() {
            return (Vec::new(), false);
        }

        let exact_matches = self.lookup_exact_title_or_display_indices(q);
        let (tier_matches, is_exact_title) = if !exact_matches.is_empty() {
            (exact_matches, true)
        } else if let Some(variant_matches) = self.lookup_list_line_variant_indices(q) {
            (variant_matches, true)
        } else if mode == SongLookupMode::Strict {
            return (Vec::new(), false);
        } else {
            (self.lookup_flexible_fallback_indices(q), false)
        };

        (
            self.filter_indices_by_artist(tier_matches, artist_filter),
            is_exact_title,
        )
    }

    fn lookup_exact_title_or_display_indices(&self, query: &str) -> Vec<usize> {
        let title_matches = self.lookup_exact_title_indices(query);
        if !title_matches.is_empty() {
            return title_matches;
        }
        self.lookup_display_song_indices(query)
    }

    fn lookup_list_line_variant_indices(&self, query: &str) -> Option<Vec<usize>> {
        for variant in list_line_fallback_variants(query) {
            let hits = self.lookup_exact_title_or_display_indices(&variant);
            if !hits.is_empty() {
                return Some(hits);
            }
        }
        None
    }

    fn lookup_flexible_fallback_indices(&self, query: &str) -> Vec<usize> {
        let q_lower = query.to_lowercase();
        let file_match: Vec<usize> = self
            .songs
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.path
                    .file_stem()
                    .and_then(|st| st.to_str())
                    .is_some_and(|st| st.to_lowercase() == q_lower)
                    || s.path
                        .file_name()
                        .and_then(|fnm| fnm.to_str())
                        .is_some_and(|fnm| fnm.to_lowercase() == q_lower)
            })
            .map(|(i, _)| i)
            .collect();
        if !file_match.is_empty() {
            return file_match;
        }

        let substring: Vec<usize> = self
            .songs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.title.to_lowercase().contains(&q_lower))
            .map(|(i, _)| i)
            .collect();
        if !substring.is_empty() {
            return substring;
        }

        let q_norm = normalize_display_separators(query);
        if !q_norm.contains(" - ") {
            return Vec::new();
        }
        let q_norm_lower = q_norm.to_lowercase();
        for reversed in [false, true] {
            let display_sub: Vec<usize> = self
                .songs
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    song_display_text(s, reversed)
                        .to_lowercase()
                        .contains(&q_norm_lower)
                })
                .map(|(i, _)| i)
                .collect();
            if !display_sub.is_empty() {
                return display_sub;
            }
        }
        Vec::new()
    }

    fn filter_indices_by_artist(
        &self,
        indices: Vec<usize>,
        artist_filter: Option<&str>,
    ) -> Vec<usize> {
        let Some(raw_artist) = artist_filter else {
            return indices;
        };
        let a = raw_artist.trim();
        if a.is_empty() || indices.is_empty() {
            return Vec::new();
        }
        let a_lower = a.to_lowercase();

        let exact_artist: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| {
                self.songs[i]
                    .artist
                    .as_deref()
                    .is_some_and(|art| art.to_lowercase() == a_lower)
            })
            .collect();
        if !exact_artist.is_empty() {
            return exact_artist;
        }

        indices
            .into_iter()
            .filter(|&i| {
                self.songs[i]
                    .artist
                    .as_deref()
                    .is_some_and(|art| art.to_lowercase().contains(&a_lower))
            })
            .collect()
    }

    fn resolve_numeric_play_target(&self, q: &str, kind: TargetKind) -> Result<PlayResolution> {
        match q.parse::<usize>() {
            Ok(n) if n >= 1 => match kind {
                TargetKind::Playlist => {
                    if n <= self.playlists.len() {
                        return self
                            .play_target_for_playlist_index(n - 1)
                            .map(PlayResolution::Ready);
                    }
                    self.resolve_exact_numeric_fallback(
                        q,
                        kind,
                        index_out_of_bounds_error("playlist", self.playlists.len(), &n.to_string()),
                    )
                }
                TargetKind::Song | TargetKind::Auto => {
                    if n <= self.songs.len() {
                        return self
                            .play_target_for_song_index(n - 1)
                            .map(PlayResolution::Ready);
                    }
                    self.resolve_exact_numeric_fallback(
                        q,
                        kind,
                        index_out_of_bounds_error("song", self.songs.len(), &n.to_string()),
                    )
                }
            },
            Ok(_) => self.resolve_exact_numeric_fallback(
                q,
                kind,
                TunrError::InvalidArg(INVALID_ONE_BASED_INDEX_MSG.to_string()),
            ),
            Err(_) => {
                let err = match kind {
                    TargetKind::Playlist => index_out_of_bounds_error(
                        "playlist",
                        self.playlists.len(),
                        &format!("'{q}'"),
                    ),
                    TargetKind::Song | TargetKind::Auto => {
                        index_out_of_bounds_error("song", self.songs.len(), &format!("'{q}'"))
                    }
                };
                self.resolve_exact_numeric_fallback(q, kind, err)
            }
        }
    }

    fn resolve_exact_numeric_fallback(
        &self,
        q: &str,
        kind: TargetKind,
        on_none: TunrError,
    ) -> Result<PlayResolution> {
        match kind {
            TargetKind::Playlist => match self.lookup_playlist(q) {
                LookupMatch::None => Err(on_none),
                _ => self
                    .resolve_playlist_play_target(q)
                    .map(PlayResolution::Ready),
            },
            TargetKind::Song => {
                let song_indices = self
                    .lookup_song_indices_with_tier(q, None, SongLookupMode::Strict)
                    .0;
                if song_indices.is_empty() {
                    Err(on_none)
                } else {
                    self.resolve_song_indices_to_resolution(q, None, song_indices)
                }
            }
            TargetKind::Auto => {
                let song_indices = self
                    .lookup_song_indices_with_tier(q, None, SongLookupMode::Strict)
                    .0;
                let playlist_match = self.lookup_playlist(q);
                let has_song = !song_indices.is_empty();
                let has_playlist = !matches!(playlist_match, LookupMatch::None);

                if has_song && has_playlist {
                    Err(TunrError::Ambiguous(format!(
                        "'{q}' matches both a song and a playlist; specify `--song` or `--playlist`"
                    )))
                } else if has_playlist {
                    self.resolve_playlist_play_target(q)
                        .map(PlayResolution::Ready)
                } else if has_song {
                    self.resolve_song_indices_to_resolution(q, None, song_indices)
                } else {
                    Err(on_none)
                }
            }
        }
    }

    fn resolve_playlist_play_target(&self, query: &str) -> Result<PlayTarget> {
        self.resolve_playlist_play_target_at(query, None)
    }

    fn resolve_playlist_play_target_at(
        &self,
        query: &str,
        one_based_index: Option<usize>,
    ) -> Result<PlayTarget> {
        let idx = self.resolve_unique_playlist_index(query)?;
        self.playlist_play_target_at(idx, one_based_index)
    }

    fn playlist_play_target_at(
        &self,
        pl_idx: usize,
        one_based_index: Option<usize>,
    ) -> Result<PlayTarget> {
        let playlist = self.playlists[pl_idx].clone();
        let songs: Vec<Song> = self
            .resolve_playable_playlist_entries(pl_idx)
            .into_iter()
            .map(|(_, song)| song)
            .collect();
        if songs.is_empty() {
            return Err(TunrError::NotFound(format!(
                "playlist '{}' contains no playable songs",
                playlist.name
            )));
        }
        let start_index = match one_based_index {
            Some(idx) => {
                if idx == 0 {
                    return Err(TunrError::InvalidArg(
                        INVALID_ONE_BASED_INDEX_MSG.to_string(),
                    ));
                }
                if idx > songs.len() {
                    return Err(TunrError::NotFound(format!(
                        "playlist '{}' track index {idx} out of bounds (playlist has {} track(s))",
                        playlist.name,
                        songs.len()
                    )));
                }
                Some(idx - 1)
            }
            None => None,
        };
        Ok(PlayTarget::Playlist {
            playlist,
            songs,
            start_index,
        })
    }

    fn resolve_unique_song_index(&self, query: &str) -> Result<usize> {
        match self.lookup_song(query) {
            LookupMatch::One(idx) => Ok(idx),
            LookupMatch::None => Err(TunrError::NotFound(format!(
                "no song found matching '{query}'"
            ))),
            LookupMatch::Multiple(matches) => Err(TunrError::Ambiguous(format!(
                "multiple songs match '{query}': {}",
                matches.join(", ")
            ))),
        }
    }

    fn resolve_unique_playlist_index(&self, query: &str) -> Result<usize> {
        match self.lookup_playlist(query) {
            LookupMatch::One(idx) => Ok(idx),
            LookupMatch::None => Err(TunrError::NotFound(format!(
                "no playlist found matching '{query}'"
            ))),
            LookupMatch::Multiple(matches) => Err(TunrError::Ambiguous(format!(
                "multiple playlists match '{query}': {}",
                matches.join(", ")
            ))),
        }
    }

    fn lookup_exact_title_indices(&self, query: &str) -> Vec<usize> {
        let exact: Vec<usize> = self
            .songs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.title == query)
            .map(|(i, _)| i)
            .collect();
        if !exact.is_empty() {
            return exact;
        }

        let needle = query.to_lowercase();
        self.songs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.title.to_lowercase() == needle)
            .map(|(i, _)| i)
            .collect()
    }

    fn lookup_display_song_indices(&self, query: &str) -> Vec<usize> {
        let q_norm = normalize_display_separators(query);
        if !q_norm.contains(" - ") {
            return Vec::new();
        }
        let q_lower = q_norm.to_lowercase();

        for reversed in [false, true] {
            let mut exact = Vec::new();
            let mut ci = Vec::new();
            for (idx, song) in self.songs.iter().enumerate() {
                let display = song_display_text(song, reversed);
                if display == q_norm {
                    exact.push(idx);
                } else if exact.is_empty() && display.to_lowercase() == q_lower {
                    ci.push(idx);
                }
            }
            if !exact.is_empty() {
                return exact;
            }
            if !ci.is_empty() {
                return ci;
            }
        }

        Vec::new()
    }

    fn indices_to_lookup_match(&self, indices: Vec<usize>) -> LookupMatch {
        match indices.len() {
            0 => LookupMatch::None,
            1 => LookupMatch::One(indices[0]),
            _ => LookupMatch::Multiple(
                indices
                    .into_iter()
                    .map(|i| {
                        let song = &self.songs[i];
                        format_song_label(&song.title, song.artist.as_deref(), &song.path)
                    })
                    .collect(),
            ),
        }
    }

    fn lookup_song(&self, query: &str) -> LookupMatch {
        let matches = self
            .lookup_song_indices_with_tier(query, None, SongLookupMode::Strict)
            .0;
        self.indices_to_lookup_match(matches)
    }

    fn lookup_playlist(&self, query: &str) -> LookupMatch {
        let exact: Vec<usize> = self
            .playlists
            .iter()
            .enumerate()
            .filter(|(_, p)| p.name == query)
            .map(|(i, _)| i)
            .collect();
        if !exact.is_empty() {
            return match exact.len() {
                1 => LookupMatch::One(exact[0]),
                _ => LookupMatch::Multiple(
                    exact
                        .into_iter()
                        .map(|i| self.playlists[i].name.clone())
                        .collect(),
                ),
            };
        }

        let needle = query.to_lowercase();
        let ci: Vec<usize> = self
            .playlists
            .iter()
            .enumerate()
            .filter(|(_, p)| p.name.to_lowercase() == needle)
            .map(|(i, _)| i)
            .collect();
        match ci.len() {
            0 => LookupMatch::None,
            1 => LookupMatch::One(ci[0]),
            _ => LookupMatch::Multiple(
                ci.into_iter()
                    .map(|i| self.playlists[i].name.clone())
                    .collect(),
            ),
        }
    }
}

fn probe_song(canonical: PathBuf) -> Result<Song> {
    let probe = probe_track(&canonical)?;
    let (title, artist) =
        normalize_track_metadata(probe.title.as_deref(), probe.artist.as_deref(), &canonical);
    let album = clean_opt(probe.album);
    Ok(Song {
        id: Uuid::new_v4().to_string(),
        title,
        artist,
        album,
        path: canonical,
        duration_secs: probe.duration_secs,
    })
}

fn duration_bucket(duration_secs: Option<f64>) -> Option<i64> {
    duration_secs
        .filter(|d| d.is_finite())
        .map(|d| (d * 10.0).round() as i64)
}

fn apply_relink(target: &mut Song, candidate: &Song) {
    target.path = candidate.path.clone();
    target.title = candidate.title.clone();
    target.artist = candidate.artist.clone();
    target.album = candidate.album.clone();
    target.duration_secs = candidate.duration_secs;
}

fn match_unique_moves<K>(
    lib: &Library,
    missing_indices: &[usize],
    candidates: &[Song],
    key_fn: impl Fn(&Song) -> Option<K>,
) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>)
where
    K: Eq + std::hash::Hash,
{
    let mut missing_by_key: HashMap<K, Vec<usize>> = HashMap::new();
    for &song_idx in missing_indices {
        if let Some(key) = key_fn(&lib.songs[song_idx]) {
            missing_by_key.entry(key).or_default().push(song_idx);
        }
    }

    let mut cands_by_key: HashMap<K, Vec<usize>> = HashMap::new();
    for (cand_idx, cand) in candidates.iter().enumerate() {
        if let Some(key) = key_fn(cand) {
            cands_by_key.entry(key).or_default().push(cand_idx);
        }
    }

    let mut matches = Vec::new();
    let mut matched_songs = HashSet::new();
    let mut matched_cands = HashSet::new();

    for (key, m_list) in missing_by_key {
        if m_list.len() == 1
            && let Some(c_list) = cands_by_key.get(&key)
            && c_list.len() == 1
        {
            let song_idx = m_list[0];
            let cand_idx = c_list[0];
            matches.push((song_idx, cand_idx));
            matched_songs.insert(song_idx);
            matched_cands.insert(cand_idx);
        }
    }

    let remaining_missing = missing_indices
        .iter()
        .copied()
        .filter(|idx| !matched_songs.contains(idx))
        .collect();
    let remaining_candidates = (0..candidates.len())
        .filter(|idx| !matched_cands.contains(idx))
        .collect();

    (matches, remaining_missing, remaining_candidates)
}

fn walk_audio_files(
    root: &Path,
    skip_dirs: &[PathBuf],
    skip_dir_names: &[&str],
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let canon_skip_dirs: Vec<PathBuf> = skip_dirs
        .iter()
        .map(|d| canonicalize_clean(d).unwrap_or_else(|_| d.to_path_buf()))
        .collect();

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().is_dir() {
                if skip_dir_names.iter().any(|name| entry.file_name() == *name) {
                    return false;
                }
                if !canon_skip_dirs.is_empty()
                    && (canon_skip_dirs.iter().any(|skip| skip == entry.path())
                        || canonicalize_clean(entry.path())
                            .is_ok_and(|canon| canon_skip_dirs.iter().any(|skip| skip == &canon)))
                {
                    return false;
                }
            }
            true
        })
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                if err.depth() == 0 {
                    return Err(TunrError::Io(std::io::Error::other(format!(
                        "failed walking {}: {err}",
                        root.display()
                    ))));
                }
                let entry_path = err
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| root.display().to_string());
                skipped.push(format!("{entry_path}: {err}"));
                continue;
            }
        };
        if entry.file_type().is_symlink() {
            if is_supported_extension(entry.path()) || entry.path().extension().is_none() {
                skipped.push(format!("{}: skipped symlink", entry.path().display()));
            }
            continue;
        }
        if entry.file_type().is_file() && is_supported_extension(entry.path()) {
            match canonicalize_clean(entry.path()) {
                Ok(canonical) => files.push(canonical),
                Err(err) => skipped.push(format!("{}: {err}", entry.path().display())),
            }
        }
    }
    files.sort();
    files.dedup();
    Ok((files, skipped))
}

fn collect_candidate_files(path: &Path, folder: bool) -> Result<(Vec<PathBuf>, Vec<String>)> {
    if !folder {
        let canonical = canonicalize_clean(path)?;
        if !is_supported_extension(&canonical) {
            return Err(TunrError::InvalidArg(format!(
                "unsupported audio file extension: {}",
                canonical.display()
            )));
        }
        return Ok((vec![canonical], Vec::new()));
    }

    let (files, skipped) = walk_audio_files(path, &[], &[])?;
    if files.is_empty() {
        return Err(TunrError::NotFound(format!(
            "no supported audio files found in {}",
            path.display()
        )));
    }
    Ok((files, skipped))
}

fn split_artist_title(text: &str) -> Option<(&str, &str)> {
    const SEPARATORS: [&str; 3] = [" - ", " – ", " — "];
    let (pos, sep) = SEPARATORS
        .iter()
        .filter_map(|sep| text.find(sep).map(|idx| (idx, *sep)))
        .min_by_key(|(idx, _)| *idx)?;

    let left = text[..pos].trim();
    let right = text[pos + sep.len()..].trim();
    if left.is_empty() || right.is_empty() || left.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((left, right))
}

pub(crate) fn normalize_track_metadata(
    raw_title: Option<&str>,
    raw_artist: Option<&str>,
    path: &Path,
) -> (String, Option<String>) {
    let base_title = raw_title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| fallback_title(path));
    let base_artist = raw_artist
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    if let Some((left, right)) = split_artist_title(&base_title) {
        let artist_matches_left = base_artist
            .as_deref()
            .is_some_and(|a| a.to_lowercase() == left.to_lowercase());
        let title_matches_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::trim)
            .is_some_and(|stem| stem.to_lowercase() == base_title.to_lowercase());

        if base_artist.is_none() || artist_matches_left || title_matches_stem {
            return (right.to_string(), Some(left.to_string()));
        }
    }

    (base_title, base_artist)
}

fn index_out_of_bounds_error(
    target_label: &str,
    count: usize,
    index_display: &str,
) -> TunrError {
    TunrError::NotFound(format!(
        "{target_label} index {index_display} out of bounds (library has {count} {target_label}(s))"
    ))
}

fn format_song_label(title: &str, artist: Option<&str>, path: &Path) -> String {
    match artist {
        Some(artist) => format!("{title} - {artist} ({})", path.display()),
        None => format!("{title} ({})", path.display()),
    }
}

fn format_ambiguous_candidate(candidate: &SongCandidate) -> String {
    format_song_label(&candidate.title, candidate.artist.as_deref(), &candidate.path)
}

fn normalize_display_separators(s: &str) -> String {
    s.replace(" — ", " - ").replace(" – ", " - ")
}

fn song_display_text(song: &Song, reversed: bool) -> String {
    let title = normalize_display_separators(&song.title);
    let artist = normalize_display_separators(song.artist.as_deref().unwrap_or("Unknown Artist"));
    if reversed {
        format!("{artist} - {title}")
    } else {
        format!("{title} - {artist}")
    }
}

/// Produce ordered fallback queries for lines copied from `tunr list` (`N. Title - Artist [MM:SS]`).
fn list_line_fallback_variants(query: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let mut push_unique = |candidate: &str| {
        if candidate != query && !variants.iter().any(|existing| existing == candidate) {
            variants.push(candidate.to_string());
        }
    };

    if let Some(without_dur) = strip_trailing_duration_tag(query) {
        push_unique(without_dur);
    }
    if let Some(without_num) = strip_leading_list_number(query) {
        push_unique(without_num);
        if let Some(without_both) = strip_trailing_duration_tag(without_num) {
            push_unique(without_both);
        }
    }

    variants
}

fn strip_trailing_duration_tag(s: &str) -> Option<&str> {
    let trimmed = s.trim_end();
    let without_close = trimmed.strip_suffix(']')?;
    let open = without_close.rfind('[')?;
    let before = &without_close[..open];
    if !before.ends_with(char::is_whitespace) {
        return None;
    }
    let prefix = before.trim_end();
    if prefix.is_empty() {
        return None;
    }
    let inside = &without_close[open + 1..];
    is_valid_duration_tag(inside).then_some(prefix)
}

fn is_valid_duration_tag(inside: &str) -> bool {
    if inside == "--:--" {
        return true;
    }
    let parts: Vec<&str> = inside.split(':').collect();
    match parts.as_slice() {
        [mm, ss] => {
            mm.len() >= 2
                && mm.bytes().all(|b| b.is_ascii_digit())
                && ss.len() == 2
                && ss.bytes().all(|b| b.is_ascii_digit())
                && ss.parse::<u8>().is_ok_and(|sec| sec < 60)
        }
        [hh, mm, ss] => {
            !hh.is_empty()
                && hh.bytes().all(|b| b.is_ascii_digit())
                && mm.len() == 2
                && mm.bytes().all(|b| b.is_ascii_digit())
                && mm.parse::<u8>().is_ok_and(|min| min < 60)
                && ss.len() == 2
                && ss.bytes().all(|b| b.is_ascii_digit())
                && ss.parse::<u8>().is_ok_and(|sec| sec < 60)
        }
        _ => false,
    }
}

fn strip_leading_list_number(s: &str) -> Option<&str> {
    let trimmed = s.trim_start();
    let dot_idx = trimmed.find('.')?;
    let num_part = &trimmed[..dot_idx];
    if num_part.is_empty()
        || !num_part.bytes().all(|b| b.is_ascii_digit())
        || !num_part.bytes().any(|b| b != b'0')
    {
        return None;
    }
    let after_dot = &trimmed[dot_idx + 1..];
    if !after_dot.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = after_dot.trim();
    (!rest.is_empty()).then_some(rest)
}

fn fallback_title(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown Track")
        .to_string()
}

fn clean_opt(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn normalize_playlist_name(playlist_name: Option<&str>) -> Result<Option<String>> {
    match playlist_name {
        Some(name) => {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(TunrError::InvalidArg(
                    "playlist name cannot be empty".to_string(),
                ));
            }
            Ok(Some(trimmed.to_string()))
        }
        None => Ok(None),
    }
}

enum PlaylistBracketIndex<'a> {
    Index {
        name: &'a str,
        one_based_index: usize,
    },
    InvalidIndex {
        name: &'a str,
    },
}

enum BracketIndexLookup<'a> {
    Target {
        playlist_name: &'a str,
        one_based_index: usize,
    },
    None,
}

/// Parse trailing `<playlist>[<index>]` bracket syntax when `[...]` is empty or numeric
/// (`+`/`-` + ASCII digits, or all ASCII digits). Non-numeric tags return `None`.
fn parse_playlist_bracket_index(raw: &str) -> Option<PlaylistBracketIndex<'_>> {
    let trimmed = raw.trim();
    let without_close = trimmed.strip_suffix(']')?;
    let open = without_close.rfind('[')?;
    let name = without_close[..open].trim();
    let inside = without_close[open + 1..].trim();

    let digits = inside.strip_prefix(['+', '-']).unwrap_or(inside);
    let is_numeric = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    if !inside.is_empty() && !is_numeric {
        return None;
    }

    match inside.parse::<usize>() {
        Ok(one_based_index) if one_based_index >= 1 => Some(PlaylistBracketIndex::Index {
            name,
            one_based_index,
        }),
        _ => Some(PlaylistBracketIndex::InvalidIndex { name }),
    }
}

fn invalid_bracket_index_error(name: &str) -> TunrError {
    if name.trim().is_empty() {
        TunrError::InvalidArg("playlist name cannot be empty".to_string())
    } else {
        TunrError::InvalidArg(INVALID_ONE_BASED_INDEX_MSG.to_string())
    }
}

fn parse_validated_bracket_index(raw: &str) -> Result<BracketIndexLookup<'_>> {
    match parse_playlist_bracket_index(raw) {
        None => Ok(BracketIndexLookup::None),
        Some(PlaylistBracketIndex::InvalidIndex { name }) => {
            Err(invalid_bracket_index_error(name))
        }
        Some(PlaylistBracketIndex::Index {
            name,
            one_based_index,
        }) => {
            if name.trim().is_empty() {
                return Err(TunrError::InvalidArg(
                    "playlist name cannot be empty".to_string(),
                ));
            }
            Ok(BracketIndexLookup::Target {
                playlist_name: name,
                one_based_index,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use super::*;

    fn temp_workspace(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tunr-lib-test-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a valid 16-bit PCM mono WAV file with `duration_secs` of a 440Hz sine tone.
    fn write_synthetic_wav(path: &Path, sample_rate: u32, duration_secs: f64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let channels: u16 = 1;
        let bits_per_sample: u16 = 16;
        let num_frames = (sample_rate as f64 * duration_secs).round() as u32;
        let block_align = channels * (bits_per_sample / 8);
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = num_frames * u32::from(block_align);
        let riff_chunk_size = 36 + data_size;

        let mut buf = Vec::with_capacity((44 + data_size) as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");

        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        buf.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());

        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_size.to_le_bytes());

        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = ((2.0 * PI * 440.0 * t).sin() * 0.25 * i16::MAX as f32) as i16;
            buf.extend_from_slice(&sample.to_le_bytes());
        }

        fs::write(path, buf).unwrap();
    }

    #[test]
    fn add_single_file_dedupes_and_extracts_duration_and_stem_title() {
        let dir = temp_workspace("single");
        let wav = dir.join("Alpha_Track.wav");
        write_synthetic_wav(&wav, 44_100, 0.25);

        let mut lib = Library::default();
        let first = lib.add_files(&wav, false, None).unwrap();
        assert_eq!(first.added_count, 1);
        assert_eq!(first.existing_count, 0);
        assert_eq!(lib.songs.len(), 1);
        assert_eq!(lib.songs[0].title, "Alpha_Track");
        let dur = lib.songs[0].duration_secs.unwrap();
        assert!((dur - 0.25).abs() < 0.02, "unexpected duration {dur}");

        // Adding the exact same file again must deduplicate by canonical path.
        let second = lib.add_files(&wav, false, None).unwrap();
        assert_eq!(second.added_count, 0);
        assert_eq!(second.existing_count, 1);
        assert_eq!(lib.songs.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_folder_recursive_sorts_and_populates_playlist_without_dupes() {
        let dir = temp_workspace("folder");
        let music = dir.join("music");
        let b_wav = music.join("sub").join("Beta.wav");
        let a_wav = music.join("Alpha.wav");
        let txt = music.join("notes.txt");

        write_synthetic_wav(&b_wav, 44_100, 0.2);
        write_synthetic_wav(&a_wav, 44_100, 0.2);
        fs::write(&txt, b"ignore me").unwrap();

        let mut lib = Library::default();
        let summary = lib.add_files(&music, true, Some("Mix")).unwrap();
        assert_eq!(summary.added_count, 2);
        assert_eq!(summary.playlist_added_count, 2);
        assert_eq!(lib.songs.len(), 2);
        // Sorted deterministically by canonical path: music/Alpha.wav < music/sub/Beta.wav
        assert_eq!(lib.songs[0].title, "Alpha");
        assert_eq!(lib.songs[1].title, "Beta");
        assert_eq!(lib.playlists.len(), 1);
        assert_eq!(lib.playlists[0].name, "Mix");
        assert_eq!(lib.playlists[0].song_ids.len(), 2);

        // Re-adding one song to the same playlist must not duplicate its song_id.
        let again = lib.add_files(&a_wav, false, Some("Mix")).unwrap();
        assert_eq!(again.added_count, 0);
        assert_eq!(again.existing_count, 1);
        assert_eq!(again.playlist_added_count, 0);
        assert_eq!(lib.playlists[0].song_ids.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_playlist_is_atomic_and_case_insensitive_with_exact_preference() {
        let dir = temp_workspace("create");
        let s1 = dir.join("First Song.wav");
        let s2 = dir.join("Second Song.wav");
        write_synthetic_wav(&s1, 44_100, 0.15);
        write_synthetic_wav(&s2, 44_100, 0.15);

        let mut lib = Library::default();
        lib.add_files(&dir, true, None).unwrap();

        // Missing song fails atomically without adding playlist.
        let missing_err = lib
            .create_playlist(
                "Favorites",
                &["First Song".to_string(), "NoSuchSong".to_string()],
            )
            .unwrap_err();
        assert!(matches!(missing_err, TunrError::NotFound(msg) if msg.contains("NoSuchSong")));
        assert!(lib.playlists.is_empty());

        // Case-insensitive match succeeds.
        let pl = lib
            .create_playlist(
                "Favorites",
                &["first song".to_string(), "Second Song".to_string()],
            )
            .unwrap();
        assert_eq!(pl.name, "Favorites");
        assert_eq!(pl.song_ids.len(), 2);

        // Duplicate playlist fails cleanly.
        let dup_err = lib
            .create_playlist("favorites", &["First Song".to_string()])
            .unwrap_err();
        assert!(matches!(dup_err, TunrError::AlreadyExists(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_song_strips_playlist_references_and_keeps_disk_file_intact() {
        let dir = temp_workspace("delete");
        let s1 = dir.join("One.wav");
        let s2 = dir.join("Two.wav");
        write_synthetic_wav(&s1, 44_100, 0.15);
        write_synthetic_wav(&s2, 44_100, 0.15);

        let mut lib = Library::default();
        lib.add_files(&dir, true, Some("Roadtrip")).unwrap();
        assert_eq!(lib.playlists[0].song_ids.len(), 2);

        let outcome = lib.delete_target("One", TargetKind::Auto).unwrap();
        assert!(matches!(outcome, DeleteOutcome::Song(ref s) if s.title == "One"));
        assert_eq!(lib.songs.len(), 1);
        assert_eq!(lib.playlists[0].song_ids.len(), 1);
        // Audio file on disk MUST NOT be deleted.
        assert!(s1.exists());
        assert!(s2.exists());

        // Delete playlist removes only the playlist entry.
        let pl_out = lib.delete_target("Roadtrip", TargetKind::Auto).unwrap();
        assert!(matches!(pl_out, DeleteOutcome::Playlist(ref p) if p.name == "Roadtrip"));
        assert!(lib.playlists.is_empty());
        assert_eq!(lib.songs.len(), 1);
        assert!(s2.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collision_between_song_and_playlist_requires_disambiguation() {
        let dir = temp_workspace("collision");
        let s1 = dir.join("Aurora.wav");
        let s2 = dir.join("Borealis.wav");
        write_synthetic_wav(&s1, 44_100, 0.15);
        write_synthetic_wav(&s2, 44_100, 0.15);

        let mut lib = Library::default();
        lib.add_files(&dir, true, None).unwrap();
        // Playlist named "Aurora" collides with song "Aurora".
        lib.create_playlist("Aurora", &["Borealis".to_string()])
            .unwrap();

        assert!(matches!(
            lib.resolve_play_target("Aurora", TargetKind::Auto),
            Err(TunrError::Ambiguous(_))
        ));
        assert!(matches!(
            lib.delete_target("Aurora", TargetKind::Auto),
            Err(TunrError::Ambiguous(_))
        ));

        let song_play = lib
            .resolve_play_target("Aurora", TargetKind::Song)
            .unwrap();
        assert!(matches!(song_play, PlayTarget::Song { ref selected, .. } if selected.title == "Aurora"));

        let pl_play = lib
            .resolve_play_target("Aurora", TargetKind::Playlist)
            .unwrap();
        assert!(matches!(pl_play, PlayTarget::Playlist { ref songs, .. } if songs.len() == 1 && songs[0].title == "Borealis"));

        // Delete playlist specifically leaves song intact.
        let del = lib.delete_target("Aurora", TargetKind::Playlist).unwrap();
        assert!(matches!(del, DeleteOutcome::Playlist(_)));
        assert_eq!(lib.songs.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_play_target_rotates_library_starting_at_selected_song() {
        let dir = temp_workspace("rotate");
        write_synthetic_wav(&dir.join("01_A.wav"), 44_100, 0.1);
        write_synthetic_wav(&dir.join("02_B.wav"), 44_100, 0.1);
        write_synthetic_wav(&dir.join("03_C.wav"), 44_100, 0.1);

        let mut lib = Library::default();
        lib.add_files(&dir, true, None).unwrap();

        let target = lib.resolve_play_target("02_B", TargetKind::Song).unwrap();
        let (queue, start_index, pl_name) = target.into_play_parts();
        assert_eq!(start_index, Some(0));
        assert_eq!(pl_name, None);
        let titles: Vec<&str> = queue.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["02_B", "03_C", "01_A"]);

        lib.create_playlist("All", &["01_A".to_string(), "02_B".to_string()])
            .unwrap();
        let pl_target = lib.resolve_play_target("All", TargetKind::Playlist).unwrap();
        let (pl_queue, pl_start_index, pl_name) = pl_target.into_play_parts();
        assert_eq!(pl_start_index, None);
        assert_eq!(pl_name.as_deref(), Some("All"));
        let pl_titles: Vec<&str> = pl_queue.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(pl_titles, vec!["01_A", "02_B"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_roundtrip_leaves_no_tmp_file() {
        let dir = temp_workspace("persist");
        let lib_path = dir.join("nested").join("library.json");
        let wav = dir.join("Track.wav");
        write_synthetic_wav(&wav, 44_100, 0.1);

        let mut lib = Library::default();
        lib.add_files(&wav, false, Some("Saved")).unwrap();
        lib.save(&lib_path).unwrap();

        assert!(lib_path.exists());
        assert!(!lib_path.with_file_name("library.json.tmp").exists());

        let reloaded = Library::load(&lib_path).unwrap();
        assert_eq!(reloaded, lib);

        // Missing file loads default empty library cleanly.
        let missing = Library::load(&dir.join("absent.json")).unwrap();
        assert_eq!(missing, Library::default());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_library_update_has_zero_lost_updates_and_no_tmp() {
        let dir = temp_workspace("concurrent-update");
        let lib_dir = dir.join("playlists");
        let lib_path = lib_dir.join("library.json");

        let track_count = 8usize;
        let wav_paths: Vec<PathBuf> = (0..track_count)
            .map(|i| {
                let p = dir.join(format!("Track_{i:02}.wav"));
                write_synthetic_wav(&p, 44_100, 0.1);
                p
            })
            .collect();

        std::thread::scope(|scope| {
            for (i, wav_path) in wav_paths.iter().enumerate() {
                let lib_path = &lib_path;
                scope.spawn(move || {
                    let summary = Library::update(lib_path, |lib| {
                        lib.add_files(wav_path, false, Some("Shared"))
                    })
                    .unwrap();
                    assert_eq!(
                        summary.added_count, 1,
                        "thread {i} expected to add 1 song, got {summary:?}"
                    );
                });
            }

            for _ in 0..2 {
                let lib_path = &lib_path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        let snapshot = Library::load(lib_path).unwrap();
                        assert!(snapshot.songs.len() <= track_count);
                    }
                });
            }
        });

        let final_lib = Library::load(&lib_path).unwrap();
        assert_eq!(final_lib.songs.len(), track_count);
        for i in 0..track_count {
            let title = format!("Track_{i:02}");
            assert!(
                final_lib.songs.iter().any(|s| s.title == title),
                "missing track {title} after concurrent updates"
            );
        }

        assert_eq!(final_lib.playlists.len(), 1);
        assert_eq!(final_lib.playlists[0].name, "Shared");
        assert_eq!(final_lib.playlists[0].song_ids.len(), track_count);

        // Mutator errors must not persist partial changes or leave temp files.
        let err = Library::update(&lib_path, |lib| {
            lib.songs.clear();
            Err::<(), _>(TunrError::InvalidArg("abort update".to_string()))
        })
        .unwrap_err();
        assert!(matches!(err, TunrError::InvalidArg(_)));
        let after_err = Library::load(&lib_path).unwrap();
        assert_eq!(after_err.songs.len(), track_count);

        let leftover_tmps: Vec<PathBuf> = fs::read_dir(&lib_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tmp"))
            })
            .collect();
        assert!(leftover_tmps.is_empty(), "leftover tmp files: {leftover_tmps:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn folder_walk_reports_unfollowed_symlinks_without_aborting_valid_tracks() {
        let dir = temp_workspace("symlink-skip");
        let valid = dir.join("Valid.wav");
        write_synthetic_wav(&valid, 22_050, 0.25);

        let external = dir.join("External.wav");
        write_synthetic_wav(&external, 22_050, 0.25);
        let linked_dir = dir.join("music_root");
        fs::create_dir_all(&linked_dir).unwrap();
        let nested_valid = linked_dir.join("Inside.wav");
        write_synthetic_wav(&nested_valid, 22_050, 0.25);
        std::os::unix::fs::symlink(&external, linked_dir.join("LinkedTrack.wav")).unwrap();

        let mut lib = Library::default();
        let summary = lib.add_files(&linked_dir, true, None).unwrap();
        assert_eq!(summary.added_count, 1);
        assert_eq!(summary.added_titles, vec!["Inside".to_string()]);
        assert!(
            summary
                .skipped_files
                .iter()
                .any(|msg| msg.contains("skipped symlink") && msg.contains("LinkedTrack.wav")),
            "expected skipped symlink entry, got {:?}",
            summary.skipped_files
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_candidate_files_errors_when_folder_missing() {
        let missing = std::env::temp_dir().join(format!("tunr-missing-{}", Uuid::new_v4()));
        let err = collect_candidate_files(&missing, true).unwrap_err();
        assert!(matches!(err, TunrError::Io(_)));
    }

    #[test]
    fn normalize_track_metadata_handles_separators_numeric_prefixes_and_youtube_uploader_tags() {
        // Untagged filename splits on first hyphen/en-dash/em-dash.
        assert_eq!(
            normalize_track_metadata(None, None, Path::new("/music/Artist A - Song One.mp3")),
            ("Song One".to_string(), Some("Artist A".to_string()))
        );
        assert_eq!(
            normalize_track_metadata(None, None, Path::new("/music/Artist B – Song Two.mp3")),
            ("Song Two".to_string(), Some("Artist B".to_string()))
        );
        assert_eq!(
            normalize_track_metadata(None, None, Path::new("/music/Artist C — Song Three.flac")),
            ("Song Three".to_string(), Some("Artist C".to_string()))
        );

        // Numeric track prefixes like "01 - Intro" must NOT split as artist.
        assert_eq!(
            normalize_track_metadata(None, None, Path::new("/music/01 - Intro.wav")),
            ("01 - Intro".to_string(), None)
        );

        // yt-dlp pattern: raw_title equals file_stem while raw_artist is the uploader channel.
        assert_eq!(
            normalize_track_metadata(
                Some("h6itam · ICEDMANE · DYSMANE - CANTO DE LUNA (Extended)"),
                Some("SUKA."),
                Path::new("/music/h6itam · ICEDMANE · DYSMANE - CANTO DE LUNA (Extended).mp3"),
            ),
            (
                "CANTO DE LUNA (Extended)".to_string(),
                Some("h6itam · ICEDMANE · DYSMANE".to_string())
            )
        );

        // Matching artist prefix on left splits cleanly.
        assert_eq!(
            normalize_track_metadata(
                Some("Daft Punk - Harder Better Faster"),
                Some("daft punk"),
                Path::new("/music/track_04.mp3"),
            ),
            (
                "Harder Better Faster".to_string(),
                Some("Daft Punk".to_string())
            )
        );

        // Distinct embedded artist & non-matching stem keeps title intact.
        assert_eq!(
            normalize_track_metadata(
                Some("Part I - Dawn"),
                Some("Real Composer"),
                Path::new("/music/01_dawn.mp3"),
            ),
            ("Part I - Dawn".to_string(), Some("Real Composer".to_string()))
        );
    }

    #[test]
    fn legacy_library_json_normalizes_on_load_and_persists_on_update() {
        let dir = temp_workspace("legacy-norm");
        let lib_path = dir.join("library.json");
        let extra = dir.join("Extra.wav");
        write_synthetic_wav(&extra, 22_050, 0.1);

        let legacy = Library {
            version: LIBRARY_VERSION,
            songs: vec![Song {
                id: "s1".to_string(),
                title: "Artist X - Legacy Track".to_string(),
                artist: Some("ChannelUploader".to_string()),
                album: None,
                path: PathBuf::from("/music/Artist X - Legacy Track.mp3"),
                duration_secs: Some(120.0),
            }],
            playlists: Vec::new(),
        };
        legacy.save(&lib_path).unwrap();

        let loaded = Library::load(&lib_path).unwrap();
        assert_eq!(loaded.songs[0].title, "Legacy Track");
        assert_eq!(loaded.songs[0].artist.as_deref(), Some("Artist X"));

        // Running Library::update persists the normalized fields to disk.
        Library::update(&lib_path, |lib| lib.add_files(&extra, false, None).map(|_| ())).unwrap();
        let raw = fs::read_to_string(&lib_path).unwrap();
        let raw_parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(raw_parsed["songs"][0]["title"], "Legacy Track");
        assert_eq!(raw_parsed["songs"][0]["artist"], "Artist X");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_play_target_with_artist_handles_tiers_ambiguity_and_filtering() {
        let lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                Song {
                    id: "1".to_string(),
                    title: "Echoes".to_string(),
                    artist: Some("Artist One".to_string()),
                    album: None,
                    path: PathBuf::from("/music/one_echoes.mp3"),
                    duration_secs: Some(100.0),
                },
                Song {
                    id: "2".to_string(),
                    title: "Echoes".to_string(),
                    artist: Some("Artist Two".to_string()),
                    album: Some("Album B".to_string()),
                    path: PathBuf::from("/music/two_echoes.mp3"),
                    duration_secs: Some(200.0),
                },
                Song {
                    id: "3".to_string(),
                    title: "Midnight Echoes Remix".to_string(),
                    artist: Some("Artist One".to_string()),
                    album: None,
                    path: PathBuf::from("/music/03_remix_file.flac"),
                    duration_secs: Some(150.0),
                },
            ],
            playlists: vec![Playlist {
                name: "Midnight".to_string(),
                song_ids: vec!["3".to_string()],
            }],
        };

        // Same title across two artists returns AmbiguousSongs with full metadata.
        let res = lib
            .resolve_play_target_with_artist("echoes", TargetKind::Auto, None)
            .unwrap();
        match res {
            PlayResolution::AmbiguousSongs { query, candidates } => {
                assert_eq!(query, "echoes");
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].index, 0);
                assert_eq!(candidates[0].artist.as_deref(), Some("Artist One"));
                assert_eq!(candidates[1].index, 1);
                assert_eq!(candidates[1].artist.as_deref(), Some("Artist Two"));
            }
            other => panic!("expected AmbiguousSongs, got {other:?}"),
        }

        // Legacy resolve_play_target surfaces artist labels in Ambiguous error.
        let amb_err = lib.resolve_play_target("Echoes", TargetKind::Song).unwrap_err();
        assert!(matches!(amb_err, TunrError::Ambiguous(ref msg) if msg.contains("Artist One") && msg.contains("Artist Two")));

        // --artist filter narrows to unique candidate.
        let filtered = lib
            .resolve_play_target_with_artist("echoes", TargetKind::Auto, Some("artist two"))
            .unwrap();
        match filtered {
            PlayResolution::Ready(PlayTarget::Song { selected, queue }) => {
                assert_eq!(selected.id, "2");
                assert_eq!(queue.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["2", "3", "1"]);
            }
            other => panic!("expected Ready song 2, got {other:?}"),
        }

        // Non-matching artist filter errors with NotFound instead of falling through to substring tier.
        let not_found = lib
            .resolve_play_target_with_artist("echoes", TargetKind::Song, Some("Artist Three"))
            .unwrap_err();
        assert!(matches!(not_found, TunrError::NotFound(_)));

        // Substring match works when exact tiers have no match.
        let sub = lib
            .resolve_play_target_with_artist("Remix", TargetKind::Song, None)
            .unwrap();
        assert!(matches!(sub, PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "3"));

        // Stem / filename match works as final tier.
        let stem = lib
            .resolve_play_target_with_artist("03_remix_file", TargetKind::Song, None)
            .unwrap();
        assert!(matches!(stem, PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "3"));

        // Exact playlist name beats substring song match in TargetKind::Auto.
        let pl = lib
            .resolve_play_target_with_artist("Midnight", TargetKind::Auto, None)
            .unwrap();
        assert!(matches!(pl, PlayResolution::Ready(PlayTarget::Playlist { ref playlist, .. }) if playlist.name == "Midnight"));

        // play_target_for_song_index rotates queue and rejects out-of-bounds.
        let by_idx = lib.play_target_for_song_index(1).unwrap();
        assert!(matches!(by_idx, PlayTarget::Song { ref selected, .. } if selected.id == "2"));
        assert!(matches!(
            lib.play_target_for_song_index(99),
            Err(TunrError::NotFound(_))
        ));
    }

    fn make_test_song(id: &str, title: &str, artist: Option<&str>, path: &str) -> Song {
        Song {
            id: id.to_string(),
            title: title.to_string(),
            artist: artist.map(ToOwned::to_owned),
            album: None,
            path: PathBuf::from(path),
            duration_secs: Some(120.0),
        }
    }

    #[test]
    fn positional_numeric_query_resolves_one_based_index_before_exact_names() {
        let lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "Alpha", Some("Artist A"), "/music/alpha.mp3"),
                make_test_song("s2", "1", Some("Artist B"), "/music/one.mp3"),
                make_test_song("s3", "Summer 2024", Some("Artist C"), "/music/2024.mp3"),
            ],
            playlists: vec![
                Playlist {
                    name: "Roadtrip".to_string(),
                    song_ids: vec!["s1".to_string(), "s3".to_string()],
                },
                Playlist {
                    name: "1".to_string(),
                    song_ids: vec!["s2".to_string()],
                },
            ],
        };

        // Positional "1" picks song at 1-based index 1 (id "s1"), ahead of song titled "1" and playlist "1".
        for kind in [TargetKind::Auto, TargetKind::Song] {
            let res = lib.resolve_play_target_with_artist("1", kind, None).unwrap();
            match res {
                PlayResolution::Ready(PlayTarget::Song { selected, queue }) => {
                    assert_eq!(selected.id, "s1");
                    assert_eq!(
                        queue.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                        vec!["s1", "s2", "s3"]
                    );
                }
                other => panic!("expected Ready rotated song queue for {kind}, got {other:?}"),
            }
        }

        // Positional "2" rotates library starting at index 1 ("s2").
        let res2 = lib
            .resolve_play_target_with_artist("2", TargetKind::Auto, None)
            .unwrap();
        match res2 {
            PlayResolution::Ready(PlayTarget::Song { selected, queue }) => {
                assert_eq!(selected.id, "s2");
                assert_eq!(
                    queue.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                    vec!["s2", "s3", "s1"]
                );
            }
            other => panic!("expected Ready song s2, got {other:?}"),
        }

        // With --playlist, "1" picks playlist at 1-based index 1 ("Roadtrip"), ahead of playlist named "1".
        let pl_res = lib
            .resolve_play_target_with_artist("1", TargetKind::Playlist, None)
            .unwrap();
        match pl_res {
            PlayResolution::Ready(PlayTarget::Playlist {
                playlist,
                songs,
                start_index,
            }) => {
                assert_eq!(playlist.name, "Roadtrip");
                assert_eq!(start_index, None);
                assert_eq!(
                    songs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                    vec!["s1", "s3"]
                );
            }
            other => panic!("expected Ready playlist Roadtrip, got {other:?}"),
        }
    }

    #[test]
    fn positional_out_of_bounds_falls_back_only_to_exact_title_or_playlist() {
        let lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "1984", Some("Artist A"), "/music/nineteen84.mp3"),
                make_test_song("s2", "Summer 2024", Some("Artist B"), "/music/2024.mp3"),
            ],
            playlists: vec![
                Playlist {
                    name: "1984".to_string(),
                    song_ids: vec!["s1".to_string()],
                },
                Playlist {
                    name: "1999".to_string(),
                    song_ids: vec!["s2".to_string()],
                },
                Playlist {
                    name: "EmptyList".to_string(),
                    song_ids: vec!["missing-id".to_string()],
                },
            ],
        };

        // Out-of-bounds "1984" falls back to exact song title in Song mode.
        let song_exact = lib
            .resolve_play_target_with_artist("1984", TargetKind::Song, None)
            .unwrap();
        assert!(matches!(
            song_exact,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s1"
        ));

        // Out-of-bounds "1984" falls back to exact playlist in Playlist mode.
        let pl_exact = lib
            .resolve_play_target_with_artist("1984", TargetKind::Playlist, None)
            .unwrap();
        assert!(matches!(
            pl_exact,
            PlayResolution::Ready(PlayTarget::Playlist { ref playlist, .. }) if playlist.name == "1984"
        ));

        // Out-of-bounds "1984" in Auto mode reports Ambiguous when both exact song and playlist exist.
        assert!(matches!(
            lib.resolve_play_target_with_artist("1984", TargetKind::Auto, None),
            Err(TunrError::Ambiguous(_))
        ));

        // Out-of-bounds "1999" in Auto mode falls back to exact playlist "1999".
        let auto_pl = lib
            .resolve_play_target_with_artist("1999", TargetKind::Auto, None)
            .unwrap();
        assert!(matches!(
            auto_pl,
            PlayResolution::Ready(PlayTarget::Playlist { ref playlist, .. }) if playlist.name == "1999"
        ));

        // Out-of-bounds "2024" must NOT match substring "Summer 2024" or stem "2024.mp3".
        for kind in [TargetKind::Auto, TargetKind::Song] {
            let err = lib
                .resolve_play_target_with_artist("2024", kind, None)
                .unwrap_err();
            assert!(
                matches!(err, TunrError::NotFound(ref msg) if msg.contains("song index 2024 out of bounds")),
                "expected song out-of-bounds NotFound for {kind}, got {err:?}"
            );
        }

        // Out-of-bounds playlist index returns playlist out-of-bounds NotFound.
        let pl_oob = lib
            .resolve_play_target_with_artist("99", TargetKind::Playlist, None)
            .unwrap_err();
        assert!(
            matches!(pl_oob, TunrError::NotFound(ref msg) if msg.contains("playlist index 99 out of bounds"))
        );

        // Selecting an empty playlist by 1-based index fails with empty-playlist NotFound.
        let pl_empty = lib
            .resolve_play_target_with_artist("3", TargetKind::Playlist, None)
            .unwrap_err();
        assert!(
            matches!(pl_empty, TunrError::NotFound(ref msg) if msg.contains("contains no playable songs"))
        );

        // Overflowing numeric string returns NotFound out-of-bounds when no exact match exists.
        let overflow_err = lib
            .resolve_play_target_with_artist("999999999999999999999999999999", TargetKind::Auto, None)
            .unwrap_err();
        assert!(matches!(overflow_err, TunrError::NotFound(_)));
    }

    #[test]
    fn positional_zero_and_artist_numeric_queries_behave_consistently() {
        let mut lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "First", Some("Artist A"), "/music/first.mp3"),
                make_test_song("s2", "1", Some("Artist B"), "/music/one.mp3"),
                make_test_song("s3", "1", Some("Artist C"), "/music/one_alt.mp3"),
            ],
            playlists: Vec::new(),
        };

        // Positional "0" without an exact song/playlist named "0" returns InvalidArg.
        for kind in [TargetKind::Auto, TargetKind::Song, TargetKind::Playlist] {
            let err = lib
                .resolve_play_target_with_artist("0", kind, None)
                .unwrap_err();
            assert!(
                matches!(err, TunrError::InvalidArg(_)),
                "expected InvalidArg for '0' in {kind}, got {err:?}"
            );
        }

        // --artist with numeric query resolves via song lookup + artist filter rather than index.
        let with_artist = lib
            .resolve_play_target_with_artist("1", TargetKind::Auto, Some("Artist C"))
            .unwrap();
        assert!(matches!(
            with_artist,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s3"
        ));

        // If an exact song titled "0" exists, positional "0" resolves it in Song/Auto modes.
        lib.songs.push(make_test_song("s0", "0", Some("ZeroArtist"), "/music/zero.mp3"));
        let zero_song = lib
            .resolve_play_target_with_artist("0", TargetKind::Auto, None)
            .unwrap();
        assert!(matches!(
            zero_song,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s0"
        ));
    }

    #[test]
    fn resolve_play_index_is_strict_and_validates_bounds_zero_and_artist() {
        let lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "1984", Some("Artist A"), "/music/1984.mp3"),
                make_test_song("s2", "Two", Some("Artist B"), "/music/two.mp3"),
            ],
            playlists: vec![
                Playlist {
                    name: "1984".to_string(),
                    song_ids: vec!["s2".to_string()],
                },
                Playlist {
                    name: "Empty".to_string(),
                    song_ids: vec!["missing".to_string()],
                },
            ],
        };

        // Valid 1-based song index in Auto and Song modes.
        for kind in [TargetKind::Auto, TargetKind::Song] {
            let res = lib.resolve_play_index(2, kind, None).unwrap();
            match res {
                PlayResolution::Ready(PlayTarget::Song { selected, queue }) => {
                    assert_eq!(selected.id, "s2");
                    assert_eq!(
                        queue.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                        vec!["s2", "s1"]
                    );
                }
                other => panic!("expected Ready song s2, got {other:?}"),
            }
        }

        // Valid 1-based playlist index.
        let pl = lib.resolve_play_index(1, TargetKind::Playlist, None).unwrap();
        assert!(matches!(
            pl,
            PlayResolution::Ready(PlayTarget::Playlist {
                ref playlist,
                ref songs,
                start_index: None,
            })
            if playlist.name == "1984" && songs.len() == 1 && songs[0].id == "s2"
        ));

        // Empty playlist by index returns NotFound.
        assert!(matches!(
            lib.resolve_play_index(2, TargetKind::Playlist, None),
            Err(TunrError::NotFound(ref msg)) if msg.contains("contains no playable songs")
        ));

        // Strict index-only: out-of-bounds 1984 does NOT fall back to song/playlist named "1984".
        assert!(matches!(
            lib.resolve_play_index(1984, TargetKind::Auto, None),
            Err(TunrError::NotFound(ref msg)) if msg.contains("song index 1984 out of bounds")
        ));
        assert!(matches!(
            lib.resolve_play_index(1984, TargetKind::Playlist, None),
            Err(TunrError::NotFound(ref msg)) if msg.contains("playlist index 1984 out of bounds")
        ));

        // Index 0 is rejected with InvalidArg even if a song/playlist named "0" existed.
        assert!(matches!(
            lib.resolve_play_index(0, TargetKind::Auto, None),
            Err(TunrError::InvalidArg(_))
        ));

        // artist.is_some() is rejected with InvalidArg.
        assert!(matches!(
            lib.resolve_play_index(1, TargetKind::Song, Some("Artist A")),
            Err(TunrError::InvalidArg(ref msg)) if msg.contains("--artist cannot be used with --index")
        ));
    }

    #[test]
    fn create_playlist_matches_display_formats_dashes_and_list_decorations() {
        let mut lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "Mundo", Some("IV Of Spades"), "/music/Mundo.mp3"),
                make_test_song("s2", "After Dark", Some("Mr.Kitty"), "/music/After Dark.mp3"),
                make_test_song("s3", "Alpha_Track", None, "/music/Alpha_Track.wav"),
            ],
            playlists: Vec::new(),
        };

        let p1 = lib
            .create_playlist(
                "DisplayForward",
                &[
                    "Mundo - IV Of Spades".to_string(),
                    "After Dark – Mr.Kitty".to_string(),
                    "Alpha_Track - Unknown Artist".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(p1.song_ids, vec!["s1", "s2", "s3"]);

        let p2 = lib
            .create_playlist(
                "DisplayReversedAndDashes",
                &[
                    "mundo — iv of spades".to_string(),
                    "Mr.Kitty - After Dark".to_string(),
                    "Unknown Artist - Alpha_Track".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(p2.song_ids, vec!["s1", "s2", "s3"]);

        let p3 = lib
            .create_playlist(
                "ListCopied",
                &[
                    "1. Mundo - IV Of Spades [05:49]".to_string(),
                    "2. After Dark - Mr.Kitty [01:02:03]".to_string(),
                    "3. Alpha_Track [--:--]".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(p3.song_ids, vec!["s1", "s2", "s3"]);

        // Stripping list number must never coerce "5. 1" into 1-based numeric index 1.
        let non_numeric_strip = lib
            .create_playlist("NoCoerce", &["5. 1".to_string()])
            .unwrap_err();
        assert!(matches!(non_numeric_strip, TunrError::NotFound(_)));
    }

    #[test]
    fn create_playlist_protects_literal_hyphens_brackets_and_numbered_titles() {
        let mut lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song(
                    "s1",
                    "Skrillex and Diplo - \"Where Are Ü Now\" with Justin Bieber (Official Video)",
                    Some("Jack Ü"),
                    "/music/skrillex_fullwidth.mp3",
                ),
                make_test_song("s2", "Countdown [01:30]", None, "/music/c1.mp3"),
                make_test_song("s3", "Countdown", None, "/music/c2.mp3"),
                make_test_song("s4", "Song [Remix]", None, "/music/remix.mp3"),
                make_test_song("s5", "1. Intro", None, "/music/intro1.mp3"),
                make_test_song("s6", "Intro", None, "/music/intro.mp3"),
                make_test_song("s7", "A", Some("B"), "/music/ab.mp3"),
                make_test_song("s8", "B", Some("A"), "/music/ba.mp3"),
            ],
            playlists: Vec::new(),
        };

        let p1 = lib
            .create_playlist(
                "LiteralCases",
                &[
                    "Skrillex and Diplo - \"Where Are Ü Now\" with Justin Bieber (Official Video) - Jack Ü"
                        .to_string(),
                    "Countdown [01:30]".to_string(),
                    "Countdown [04:00]".to_string(),
                    "Song [Remix]".to_string(),
                    "1. Intro".to_string(),
                    "6. Intro".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(p1.song_ids, vec!["s1", "s2", "s3", "s4", "s5", "s6"]);

        // Forward "Title - Artist" takes priority over Reversed "Artist - Title".
        let p2 = lib
            .create_playlist("DirectionOrder", &["A - B".to_string(), "B - A".to_string()])
            .unwrap();
        assert_eq!(p2.song_ids, vec!["s7", "s8"]);

        // Non-duration bracket suffixes like "[Remix]" are never stripped.
        let unstripped = lib
            .create_playlist("NoStripBracket", &["Missing [Remix]".to_string()])
            .unwrap_err();
        assert!(matches!(unstripped, TunrError::NotFound(_)));
    }

    #[test]
    fn create_playlist_supports_indices_stem_filename_display_substring_and_dedup() {
        let mut lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "First", Some("Artist A"), "/music/first.mp3"),
                make_test_song("s2", "1", Some("Artist X"), "/music/track_one.mp3"),
                make_test_song(
                    "s3",
                    "Sanctuary",
                    Some("Joji, Justin Raisen, Daniel Wilson"),
                    "/music/Sanctuary.mp3",
                ),
                make_test_song(
                    "s4",
                    "Midnight Echoes Remix",
                    Some("Artist One"),
                    "/music/03_remix_file.flac",
                ),
                make_test_song("s5", "Echoes", Some("Artist One"), "/music/echoes_1.mp3"),
                make_test_song("s6", "Echoes", Some("Artist Two"), "/music/echoes_2.mp3"),
                make_test_song("s7", "1984", Some("Band"), "/music/nineteen84.mp3"),
                make_test_song("s8", "Summer 2024", Some("Pop"), "/music/2024.mp3"),
            ],
            playlists: Vec::new(),
        };

        // 1-based index beats literal title "1"; mixed indices and titles deduplicate song IDs.
        let p1 = lib
            .create_playlist(
                "MixedSpecifiers",
                &[
                    "1".to_string(),
                    "First".to_string(),
                    "03_remix_file".to_string(),
                    "03_remix_file.flac".to_string(),
                    "Sanctuary - Joji".to_string(),
                    "1984".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(p1.song_ids, vec!["s1", "s4", "s3", "s7"]);

        // Unique CI title substring works.
        let p2 = lib
            .create_playlist("SubstringUnique", &["remix".to_string()])
            .unwrap();
        assert_eq!(p2.song_ids, vec!["s4"]);

        // Bare artist name without " - " does not match via Tier 4b.
        let bare_artist = lib
            .create_playlist("BareArtist", &["Joji".to_string()])
            .unwrap_err();
        assert!(matches!(bare_artist, TunrError::NotFound(_)));

        // Out-of-bounds "2024" must NOT fall back to substring "Summer 2024" or stem "2024.mp3".
        let oob_2024 = lib
            .create_playlist("OobNoFuzzy", &["2024".to_string()])
            .unwrap_err();
        assert!(matches!(oob_2024, TunrError::NotFound(_)));

        // Ambiguous song returns Ambiguous with artist hints and leaves playlists unchanged.
        let amb = lib
            .create_playlist("Amb", &["echoes".to_string()])
            .unwrap_err();
        assert!(
            matches!(amb, TunrError::Ambiguous(ref msg) if msg.contains("Artist One") && msg.contains("Artist Two")),
            "expected Ambiguous with artists, got {amb:?}"
        );
        assert!(lib.playlists.iter().all(|p| p.name != "Amb"));

        // "0" without an exact song titled "0" returns InvalidArg regardless of item order.
        for items in [
            vec!["0".to_string(), "NoSuchSong".to_string()],
            vec!["NoSuchSong".to_string(), "0".to_string()],
        ] {
            let err = lib.create_playlist("ZeroErr", &items).unwrap_err();
            assert!(
                matches!(err, TunrError::InvalidArg(_)),
                "expected InvalidArg for {items:?}, got {err:?}"
            );
        }

        // Exact song titled "0" is resolved when present (and Ambiguous if duplicated).
        lib.songs
            .push(make_test_song("s0", "0", Some("ZeroA"), "/music/zero_a.mp3"));
        let p0 = lib
            .create_playlist("ExactZero", &["0".to_string()])
            .unwrap();
        assert_eq!(p0.song_ids, vec!["s0"]);

        lib.songs
            .push(make_test_song("s0b", "0", Some("ZeroB"), "/music/zero_b.mp3"));
        let amb_zero = lib
            .create_playlist("ExactZeroDup", &["0".to_string()])
            .unwrap_err();
        assert!(matches!(amb_zero, TunrError::Ambiguous(_)));
    }

    #[test]
    fn delete_target_supports_display_disambiguation_without_substring_or_numeric_index() {
        let mut lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "Echoes", Some("Artist One"), "/music/echoes_1.mp3"),
                make_test_song("s2", "Echoes", Some("Artist Two"), "/music/echoes_2.mp3"),
                make_test_song(
                    "s3",
                    "Midnight Remix",
                    Some("Artist Three"),
                    "/music/03_remix_file.flac",
                ),
                make_test_song("s4", "1", Some("Artist Four"), "/music/one.mp3"),
            ],
            playlists: Vec::new(),
        };

        // Duplicate title reports Ambiguous with artist names.
        let amb = lib.delete_target("Echoes", TargetKind::Auto).unwrap_err();
        assert!(
            matches!(amb, TunrError::Ambiguous(ref msg) if msg.contains("Artist One") && msg.contains("Artist Two")),
            "expected Ambiguous with artist hints, got {amb:?}"
        );

        // Display-format string disambiguates and deletes only the matching song.
        let deleted = lib
            .delete_target("Echoes - Artist One", TargetKind::Auto)
            .unwrap();
        assert!(matches!(deleted, DeleteOutcome::Song(ref s) if s.id == "s1"));
        assert!(lib.find_song_by_id("s1").is_none());
        assert!(lib.find_song_by_id("s2").is_some());

        // Substring and file_stem remain disallowed in delete.
        assert!(matches!(
            lib.delete_target("Remix", TargetKind::Song),
            Err(TunrError::NotFound(_))
        ));
        assert!(matches!(
            lib.delete_target("03_remix_file", TargetKind::Song),
            Err(TunrError::NotFound(_))
        ));

        // Numeric queries in delete never resolve by 1-based list index.
        assert!(matches!(
            lib.delete_target("2", TargetKind::Auto),
            Err(TunrError::NotFound(_))
        ));
        let del_one = lib.delete_target("1", TargetKind::Song).unwrap();
        assert!(matches!(del_one, DeleteOutcome::Song(ref s) if s.id == "s4"));
    }

    #[test]
    fn play_target_supports_display_format_and_preserves_auto_and_artist_invariants() {
        let lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "Mundo", Some("IV Of Spades"), "/music/Mundo.mp3"),
                make_test_song(
                    "s2",
                    "Sanctuary",
                    Some("Joji, Justin Raisen"),
                    "/music/Sanctuary.mp3",
                ),
                make_test_song("s3", "Echoes", Some("Artist One"), "/music/echoes_1.mp3"),
                make_test_song("s4", "Echoes", Some("Artist Two"), "/music/echoes_2.mp3"),
                make_test_song(
                    "s5",
                    "Echoes Extended",
                    Some("Artist Three"),
                    "/music/echoes_ext.mp3",
                ),
            ],
            playlists: vec![Playlist {
                name: "Mundo - IV Of Spades".to_string(),
                song_ids: vec!["s2".to_string()],
            }],
        };

        // Exact display string and list line resolve the song directly.
        for query in ["Mundo - IV Of Spades", "1. Mundo - IV Of Spades [05:49]"] {
            let res = lib
                .resolve_play_target_with_artist(query, TargetKind::Song, None)
                .unwrap();
            assert!(matches!(
                res,
                PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s1"
            ));
        }

        // Exact display match in Auto collides with a same-named playlist.
        assert!(matches!(
            lib.resolve_play_target_with_artist("Mundo - IV Of Spades", TargetKind::Auto, None),
            Err(TunrError::Ambiguous(_))
        ));

        // Tier 4b truncated display substring resolves "Sanctuary - Joji".
        let sub_disp = lib
            .resolve_play_target_with_artist("Sanctuary - Joji", TargetKind::Song, None)
            .unwrap();
        assert!(matches!(
            sub_disp,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s2"
        ));

        // Duplicate-title disambiguation via "Title - Artist", and no tier fallthrough on --artist mismatch.
        let disp_echoes = lib
            .resolve_play_target_with_artist("Echoes - Artist Two", TargetKind::Song, None)
            .unwrap();
        assert!(matches!(
            disp_echoes,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s4"
        ));
        assert!(matches!(
            lib.resolve_play_target_with_artist(
                "Echoes - Artist Two",
                TargetKind::Song,
                Some("Artist One")
            ),
            Err(TunrError::NotFound(_))
        ));
    }

    #[test]
    fn play_playlist_bracket_index_starts_at_track_and_preserves_literals_and_edge_cases() {
        let lib = Library {
            version: LIBRARY_VERSION,
            songs: vec![
                make_test_song("s1", "One", Some("Artist A"), "/music/one.mp3"),
                make_test_song("s2", "Two", Some("Artist B"), "/music/two.mp3"),
                make_test_song("s3", "Three", Some("Artist C"), "/music/three.mp3"),
                make_test_song("s4", "Song [2024]", Some("Artist D"), "/music/song_2024.mp3"),
                make_test_song("s5", "Song [Remix]", Some("Artist E"), "/music/song_remix.mp3"),
            ],
            playlists: vec![
                Playlist {
                    name: "MyList".to_string(),
                    song_ids: vec![
                        "dangling-id".to_string(),
                        "s1".to_string(),
                        "s2".to_string(),
                        "s3".to_string(),
                    ],
                },
                Playlist {
                    name: "Mix [2]".to_string(),
                    song_ids: vec!["s2".to_string(), "s3".to_string()],
                },
                Playlist {
                    name: "EmptyList".to_string(),
                    song_ids: vec!["missing-id".to_string()],
                },
            ],
        };

        // Both Auto and Playlist resolve 1-based track index while keeping the full playable queue.
        for kind in [TargetKind::Auto, TargetKind::Playlist] {
            let res = lib
                .resolve_play_target_with_artist("mylist[2]", kind, None)
                .unwrap();
            match res {
                PlayResolution::Ready(target @ PlayTarget::Playlist { .. }) => {
                    let (queue, start_index, pl_name) = target.into_play_parts();
                    assert_eq!(pl_name.as_deref(), Some("MyList"));
                    assert_eq!(start_index, Some(1));
                    assert_eq!(
                        queue.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                        vec!["s1", "s2", "s3"]
                    );
                }
                other => panic!("expected Ready playlist for {kind}, got {other:?}"),
            }
        }

        // Invalid bracket indices return InvalidArg.
        for bad in [
            "MyList[0]",
            "MyList[-1]",
            "MyList[]",
            "MyList[999999999999999999999999999999]",
            "[1]",
            "[]",
        ] {
            for kind in [TargetKind::Auto, TargetKind::Playlist] {
                assert!(
                    matches!(
                        lib.resolve_play_target_with_artist(bad, kind, None),
                        Err(TunrError::InvalidArg(_))
                    ),
                    "expected InvalidArg for {bad} in {kind}"
                );
            }
        }

        // Out-of-bounds and empty playlists return NotFound.
        let oob = lib
            .resolve_play_target_with_artist("MyList[4]", TargetKind::Playlist, None)
            .unwrap_err();
        assert!(
            matches!(oob, TunrError::NotFound(ref msg) if msg.contains("MyList") && msg.contains("3 track(s)"))
        );
        let empty = lib
            .resolve_play_target_with_artist("EmptyList[1]", TargetKind::Auto, None)
            .unwrap_err();
        assert!(
            matches!(empty, TunrError::NotFound(ref msg) if msg.contains("contains no playable songs"))
        );

        // Literal song titles with brackets and literal playlists with numeric brackets are preserved.
        let literal_song = lib
            .resolve_play_target_with_artist("Song [2024]", TargetKind::Auto, None)
            .unwrap();
        assert!(matches!(
            literal_song,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s4"
        ));
        let remix_song = lib
            .resolve_play_target_with_artist("Song [Remix]", TargetKind::Auto, None)
            .unwrap();
        assert!(matches!(
            remix_song,
            PlayResolution::Ready(PlayTarget::Song { ref selected, .. }) if selected.id == "s5"
        ));
        let literal_pl = lib
            .resolve_play_target_with_artist("Mix [2]", TargetKind::Auto, None)
            .unwrap();
        assert!(matches!(
            literal_pl,
            PlayResolution::Ready(PlayTarget::Playlist {
                ref playlist,
                start_index: None,
                ..
            }) if playlist.name == "Mix [2]"
        ));

        // --song and --artist never treat bracket syntax as playlist traversal.
        assert!(matches!(
            lib.resolve_play_target_with_artist("MyList[1]", TargetKind::Song, None),
            Err(TunrError::NotFound(_))
        ));
        assert!(matches!(
            lib.resolve_play_target_with_artist("MyList[1]", TargetKind::Auto, Some("Artist A")),
            Err(TunrError::NotFound(_))
        ));
    }

    #[test]
    fn delete_playlist_song_by_bracket_index_keeps_song_disk_and_empty_playlist() {
        let dir = temp_workspace("delete-pl-song");
        let f1 = dir.join("First.wav");
        let f2 = dir.join("Second.wav");
        write_synthetic_wav(&f1, 44_100, 0.1);
        write_synthetic_wav(&f2, 44_100, 0.1);

        let mut lib = Library::default();
        lib.add_files(&dir, true, Some("MyList")).unwrap();
        // Prepend a dangling ID to verify 1-based indexing aligns with playable songs only.
        lib.playlists[0].song_ids.insert(0, "dangling-id".to_string());
        lib.playlists.push(Playlist {
            name: "Mix [2]".to_string(),
            song_ids: vec![lib.songs[0].id.clone()],
        });
        lib.playlists.push(Playlist {
            name: "EmptyList".to_string(),
            song_ids: vec!["missing".to_string()],
        });
        lib.songs.push(make_test_song(
            "s-bracket",
            "Title[1]",
            Some("BracketArtist"),
            "/music/title_bracket.mp3",
        ));

        // Delete 2nd playable track ("Second") via TargetKind::Playlist.
        let out = lib.delete_target("MyList[2]", TargetKind::Playlist).unwrap();
        match out {
            DeleteOutcome::PlaylistSong {
                playlist,
                song,
                index,
            } => {
                assert_eq!(playlist.name, "MyList");
                assert_eq!(song.title, "Second");
                assert_eq!(index, 2);
            }
            other => panic!("expected PlaylistSong outcome, got {other:?}"),
        }
        assert_eq!(lib.songs.len(), 3);
        assert!(f1.exists());
        assert!(f2.exists());
        assert_eq!(
            lib.playlists[0].song_ids,
            vec!["dangling-id".to_string(), lib.songs[0].id.clone()]
        );

        // Delete remaining playable track ("First") via Auto fallback; empty playlist remains intact.
        let out_last = lib.delete_target("mylist[1]", TargetKind::Auto).unwrap();
        assert!(matches!(
            out_last,
            DeleteOutcome::PlaylistSong { ref song, index: 1, .. } if song.title == "First"
        ));
        assert_eq!(lib.playlists[0].name, "MyList");
        assert_eq!(lib.playlists[0].song_ids, vec!["dangling-id".to_string()]);
        assert!(f1.exists());

        // Invalid indices, out-of-bounds, and empty playlist error as expected.
        for bad in ["MyList[0]", "MyList[-1]", "MyList[]", "[1]"] {
            assert!(matches!(
                lib.delete_target(bad, TargetKind::Playlist),
                Err(TunrError::InvalidArg(_))
            ));
        }
        assert!(matches!(
            lib.delete_target("MyList[1]", TargetKind::Playlist),
            Err(TunrError::NotFound(ref msg)) if msg.contains("contains no songs")
        ));
        assert!(matches!(
            lib.delete_target("EmptyList[1]", TargetKind::Auto),
            Err(TunrError::NotFound(ref msg)) if msg.contains("contains no songs")
        ));

        // Literal playlist "Mix [2]" deletes the whole playlist; literal song "Title[1]" deletes the song.
        let del_pl = lib.delete_target("Mix [2]", TargetKind::Playlist).unwrap();
        assert!(matches!(del_pl, DeleteOutcome::Playlist(ref p) if p.name == "Mix [2]"));
        let del_song = lib.delete_target("Title[1]", TargetKind::Auto).unwrap();
        assert!(matches!(del_song, DeleteOutcome::Song(ref s) if s.id == "s-bracket"));
        assert!(matches!(
            lib.delete_target("MyList[1]", TargetKind::Song),
            Err(TunrError::NotFound(_))
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_song_handles_files_and_existing_library_songs_with_playlists() {
        let dir = temp_workspace("add-song");
        let f1 = dir.join("Alpha.wav");
        let f2 = dir.join("Beta.wav");
        write_synthetic_wav(&f1, 44_100, 0.1);
        write_synthetic_wav(&f2, 44_100, 0.1);

        let mut lib = Library::default();

        // Import audio file directly without a playlist.
        let s1 = lib.add_song(f1.to_str().unwrap(), None).unwrap();
        assert_eq!(s1.added_count, 1);
        assert_eq!(s1.existing_count, 0);
        assert_eq!(s1.playlist_name, None);
        assert_eq!(lib.songs.len(), 1);

        // Import audio file and create/add to playlist.
        let s2 = lib.add_song(f2.to_str().unwrap(), Some("Roadtrip")).unwrap();
        assert_eq!(s2.added_count, 1);
        assert_eq!(s2.playlist_name.as_deref(), Some("Roadtrip"));
        assert_eq!(s2.playlist_added_count, 1);
        assert_eq!(lib.playlists.len(), 1);

        // Add existing library song by 1-based index to existing playlist (case-insensitive) and deduplicate.
        let by_idx = lib.add_song("1", Some("roadtrip")).unwrap();
        assert_eq!(by_idx.added_count, 0);
        assert_eq!(by_idx.existing_count, 1);
        assert_eq!(by_idx.playlist_name.as_deref(), Some("Roadtrip"));
        assert_eq!(by_idx.playlist_added_count, 1);
        assert_eq!(lib.playlists[0].song_ids.len(), 2);

        let dup = lib.add_song("Alpha", Some("Roadtrip")).unwrap();
        assert_eq!(dup.added_count, 0);
        assert_eq!(dup.existing_count, 1);
        assert_eq!(dup.playlist_added_count, 0);
        assert_eq!(lib.playlists[0].song_ids.len(), 2);

        // Add by "Title - Artist" display string into a new playlist.
        lib.songs.push(make_test_song(
            "s-dup-1",
            "Echoes",
            Some("Artist One"),
            "/music/echoes_1.mp3",
        ));
        lib.songs.push(make_test_song(
            "s-dup-2",
            "Echoes",
            Some("Artist Two"),
            "/music/echoes_2.mp3",
        ));
        let amb = lib.add_song("Echoes", Some("Chill")).unwrap_err();
        assert!(matches!(amb, TunrError::Ambiguous(_)));
        assert_eq!(lib.playlists.len(), 1);

        let disamb = lib.add_song("Echoes - Artist Two", Some("Chill")).unwrap();
        assert_eq!(disamb.playlist_name.as_deref(), Some("Chill"));
        assert_eq!(disamb.playlist_added_count, 1);
        assert_eq!(lib.playlists[1].song_ids, vec!["s-dup-2".to_string()]);

        // Error cases: empty query, empty playlist, directory path, missing file, missing song, index 0.
        assert!(matches!(
            lib.add_song("   ", None),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            lib.add_song("Alpha", Some("   ")),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            lib.add_song(dir.to_str().unwrap(), None),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            lib.add_song("/nonexistent/track.wav", None),
            Err(TunrError::NotFound(_))
        ));
        assert!(matches!(
            lib.add_song("NoSuchSong", Some("Chill")),
            Err(TunrError::NotFound(_))
        ));
        assert!(matches!(
            lib.add_song("0", Some("Chill")),
            Err(TunrError::InvalidArg(_))
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_existing_song_resolves_playlist_and_song_by_name_or_index() {
        let mut lib = Library::default();
        lib.songs.push(make_test_song(
            "id-1",
            "Alpha",
            Some("Artist A"),
            "/music/alpha.mp3",
        ));
        lib.songs.push(make_test_song(
            "id-2",
            "Beta",
            Some("Artist B"),
            "/music/beta.mp3",
        ));
        lib.songs.push(make_test_song(
            "id-3",
            "Echoes",
            Some("One"),
            "/music/echoes1.mp3",
        ));
        lib.songs.push(make_test_song(
            "id-4",
            "Echoes",
            Some("Two"),
            "/music/echoes2.mp3",
        ));
        lib.playlists.push(Playlist {
            name: "Favorites".to_string(),
            song_ids: vec!["id-1".to_string()],
        });

        // Target playlist by 1-based index and song by 1-based index.
        let added = lib.add_existing_song("2", "1").unwrap();
        assert_eq!(added.added_count, 0);
        assert_eq!(added.existing_count, 1);
        assert_eq!(added.playlist_name.as_deref(), Some("Favorites"));
        assert_eq!(added.playlist_added_count, 1);
        assert_eq!(
            lib.playlists[0].song_ids,
            vec!["id-1".to_string(), "id-2".to_string()]
        );

        // Duplicate song in same playlist is skipped cleanly.
        let dup = lib.add_existing_song("Alpha", "favorites").unwrap();
        assert_eq!(dup.playlist_name.as_deref(), Some("Favorites"));
        assert_eq!(dup.playlist_added_count, 0);

        // Non-numeric missing playlist name creates the playlist and adds song.
        let created = lib.add_existing_song("Echoes - Two", "Chill").unwrap();
        assert_eq!(created.playlist_name.as_deref(), Some("Chill"));
        assert_eq!(created.playlist_added_count, 1);
        assert_eq!(lib.playlists.len(), 2);
        assert_eq!(lib.playlists[1].song_ids, vec!["id-4".to_string()]);

        // Error cases: index 0, out-of-bounds playlist index, missing song, ambiguous song, empty args.
        assert!(matches!(
            lib.add_existing_song("1", "0"),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            lib.add_existing_song("0", "1"),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            lib.add_existing_song("1", "9"),
            Err(TunrError::NotFound(_))
        ));
        assert!(matches!(
            lib.add_existing_song("MissingSong", "1"),
            Err(TunrError::NotFound(_))
        ));
        assert!(matches!(
            lib.add_existing_song("Echoes", "1"),
            Err(TunrError::Ambiguous(_))
        ));
        assert!(matches!(
            lib.add_existing_song("   ", "1"),
            Err(TunrError::InvalidArg(_))
        ));
        assert!(matches!(
            lib.add_existing_song("Alpha", "   "),
            Err(TunrError::InvalidArg(_))
        ));
    }

    struct WorkspaceFixture {
        root: PathBuf,
        working_dir: PathBuf,
        config_dir: PathBuf,
        playlists_dir: PathBuf,
        music_dir: PathBuf,
        library_path: PathBuf,
    }

    impl WorkspaceFixture {
        fn new(label: &str) -> Self {
            let root = temp_workspace(label);
            let working_dir = root.join("workspace");
            let config_dir = working_dir.join("config");
            let playlists_dir = working_dir.join("playlists");
            let music_dir = working_dir.join("music");
            fs::create_dir_all(&config_dir).unwrap();
            fs::create_dir_all(&playlists_dir).unwrap();
            fs::create_dir_all(&music_dir).unwrap();
            let library_path = playlists_dir.join("library.json");
            Self {
                root,
                working_dir,
                config_dir,
                playlists_dir,
                music_dir,
                library_path,
            }
        }
    }

    impl Drop for WorkspaceFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn update_if_changed_skips_rewrite_when_unmodified_and_writes_on_change() {
        let ws = WorkspaceFixture::new("if-changed");
        let lib_path = &ws.library_path;

        // Missing file is created on initial update_if_changed.
        Library::update_if_changed(lib_path, |_| Ok(())).unwrap();
        assert!(lib_path.exists());

        let track = ws.music_dir.join("Tone.wav");
        write_synthetic_wav(&track, 22_050, 0.2);

        // Mutating writes the updated library.
        Library::update_if_changed(lib_path, |lib| {
            lib.add_files(&track, false, None).map(|_| ())
        })
        .unwrap();

        let meta_before = fs::metadata(lib_path).unwrap();
        let bytes_before = fs::read(lib_path).unwrap();

        // No-op mutator must not rewrite library.json (mtime, inode, content stay identical).
        std::thread::sleep(std::time::Duration::from_millis(20));
        Library::update_if_changed(lib_path, |_| Ok(())).unwrap();

        let meta_after = fs::metadata(lib_path).unwrap();
        let bytes_after = fs::read(lib_path).unwrap();
        assert_eq!(meta_after.modified().unwrap(), meta_before.modified().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(meta_after.ino(), meta_before.ino());
        }
        assert_eq!(bytes_after, bytes_before);

        // Legacy un-normalized song triggers a write on next update_if_changed.
        Library::update(lib_path, |lib| {
            lib.songs[0].title = "Artist One - Tone".to_string();
            lib.songs[0].artist = None;
            Ok(())
        })
        .unwrap();
        let normalized_again = Library::update_if_changed(lib_path, |_| Ok(())).map(|_| {
            Library::load(lib_path).unwrap()
        }).unwrap();
        assert_eq!(normalized_again.songs[0].title, "Tone");
        assert_eq!(normalized_again.songs[0].artist.as_deref(), Some("Artist One"));
    }

    #[test]
    fn update_workspace_scans_roots_skips_internal_dirs_and_is_idempotent() {
        let ws = WorkspaceFixture::new("scan-roots");
        let separate_dl = ws.root.join("downloads");
        fs::create_dir_all(&separate_dl).unwrap();

        // Valid tracks across working_dir root, music/, subdirs, and separate download_dest.
        write_synthetic_wav(&ws.working_dir.join("RootTrack.wav"), 22_050, 0.2);
        write_synthetic_wav(&ws.music_dir.join("Album").join("MusicTrack.wav"), 22_050, 0.25);
        write_synthetic_wav(&separate_dl.join("sub").join("Downloaded.wav"), 22_050, 0.3);

        // Internal/staging directories must be ignored.
        write_synthetic_wav(&ws.config_dir.join("ShouldIgnoreConfig.wav"), 22_050, 0.2);
        write_synthetic_wav(
            &ws.playlists_dir.join("ShouldIgnorePlaylists.wav"),
            22_050,
            0.2,
        );
        write_synthetic_wav(
            &ws.working_dir.join(".tunr-dl-tmp").join("Partial.wav"),
            22_050,
            0.2,
        );
        write_synthetic_wav(
            &separate_dl.join(".tunr-dl-tmp").join("PartialDl.wav"),
            22_050,
            0.2,
        );

        let first = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            Some(&separate_dl),
        )
        .unwrap();
        assert_eq!(first.added_count, 3);
        assert_eq!(first.relinked_count, 0);
        assert_eq!(first.removed_count, 0);
        assert_eq!(first.unchanged_count, 0);
        assert!(first.skipped_files.is_empty(), "{:?}", first.skipped_files);

        let lib = Library::load(&ws.library_path).unwrap();
        let titles: Vec<&str> = lib.songs.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Downloaded", "RootTrack", "MusicTrack"]);

        // Re-running update_workspace with no changes is idempotent and does not rewrite library.json.
        let meta_before = fs::metadata(&ws.library_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            Some(&separate_dl),
        )
        .unwrap();
        assert_eq!(second.added_count, 0);
        assert_eq!(second.relinked_count, 0);
        assert_eq!(second.removed_count, 0);
        assert_eq!(second.unchanged_count, 3);
        assert!(second.skipped_files.is_empty());

        let meta_after = fs::metadata(&ws.library_path).unwrap();
        assert_eq!(meta_after.modified().unwrap(), meta_before.modified().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(meta_after.ino(), meta_before.ino());
        }
    }

    #[test]
    fn update_workspace_relinks_moved_files_prunes_deleted_and_keeps_external_songs() {
        let ws = WorkspaceFixture::new("relink-prune");
        let external_dir = ws.root.join("external");
        fs::create_dir_all(&external_dir).unwrap();

        let move_src = ws.music_dir.join("KeepAndMove.wav");
        let pass2_src = ws.music_dir.join("SameFilePass2.wav");
        let delete_src = ws.music_dir.join("DeleteMe.wav");
        let keep_src = ws.music_dir.join("StayPut.wav");
        let external_src = external_dir.join("ExternalTrack.wav");
        let external_missing = external_dir.join("ExternalMissing.wav");

        write_synthetic_wav(&move_src, 22_050, 0.25);
        write_synthetic_wav(&pass2_src, 22_050, 0.40);
        write_synthetic_wav(&delete_src, 22_050, 0.15);
        write_synthetic_wav(&keep_src, 22_050, 0.55);
        write_synthetic_wav(&external_src, 22_050, 0.30);
        write_synthetic_wav(&external_missing, 22_050, 0.35);

        // Index workspace files and external files, and build a playlist referencing them.
        Library::update(&ws.library_path, |lib| {
            lib.add_files(&ws.music_dir, true, Some("Favorites"))?;
            lib.add_files(&external_src, false, Some("Favorites"))?;
            lib.add_files(&external_missing, false, Some("Favorites"))?;
            // Adjust Pass 2 song's title in library so Pass 1 key differs while Pass 2 (file_name, duration_bucket) matches.
            let pass2_song = lib
                .songs
                .iter_mut()
                .find(|s| s.title == "SameFilePass2")
                .unwrap();
            pass2_song.title = "Custom Library Title".to_string();
            Ok(())
        })
        .unwrap();

        let before = Library::load(&ws.library_path).unwrap();
        let moved_id = before
            .songs
            .iter()
            .find(|s| s.title == "KeepAndMove")
            .unwrap()
            .id
            .clone();
        let pass2_id = before
            .songs
            .iter()
            .find(|s| s.title == "Custom Library Title")
            .unwrap()
            .id
            .clone();
        let deleted_id = before
            .songs
            .iter()
            .find(|s| s.title == "DeleteMe")
            .unwrap()
            .id
            .clone();
        let external_missing_id = before
            .songs
            .iter()
            .find(|s| s.title == "ExternalMissing")
            .unwrap()
            .id
            .clone();

        // Move two workspace tracks, delete one workspace track, and delete an external track.
        let move_dst = ws.working_dir.join("sorted").join("KeepAndMove.wav");
        fs::create_dir_all(move_dst.parent().unwrap()).unwrap();
        fs::rename(&move_src, &move_dst).unwrap();

        let pass2_dst = ws.working_dir.join("archived").join("SameFilePass2.wav");
        fs::create_dir_all(pass2_dst.parent().unwrap()).unwrap();
        fs::rename(&pass2_src, &pass2_dst).unwrap();

        fs::remove_file(&delete_src).unwrap();
        fs::remove_file(&external_missing).unwrap();

        let summary = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            None,
        )
        .unwrap();
        assert_eq!(summary.added_count, 0);
        assert_eq!(summary.relinked_count, 2);
        assert_eq!(summary.removed_count, 1);
        assert_eq!(summary.unchanged_count, 3); // StayPut + ExternalTrack + ExternalMissing

        let after = Library::load(&ws.library_path).unwrap();
        assert_eq!(after.songs.len(), 5);
        assert!(after.find_song_by_id(&deleted_id).is_none());

        let relinked_1 = after.find_song_by_id(&moved_id).unwrap();
        assert_eq!(relinked_1.path, canonicalize_clean(&move_dst).unwrap());

        let relinked_2 = after.find_song_by_id(&pass2_id).unwrap();
        assert_eq!(relinked_2.path, canonicalize_clean(&pass2_dst).unwrap());

        // External missing song outside scan_roots must remain untouched.
        assert!(after.find_song_by_id(&external_missing_id).is_some());

        // Playlist preserved relinked IDs and external IDs, while pruning deleted workspace ID.
        let fav = &after.playlists[0];
        assert_eq!(fav.name, "Favorites");
        assert!(fav.song_ids.contains(&moved_id));
        assert!(fav.song_ids.contains(&pass2_id));
        assert!(fav.song_ids.contains(&external_missing_id));
        assert!(!fav.song_ids.contains(&deleted_id));
    }

    #[test]
    fn update_workspace_does_not_relink_ambiguous_moves() {
        let ws = WorkspaceFixture::new("ambiguous-move");
        let a = ws.music_dir.join("dir_a").join("Dup.wav");
        let b = ws.music_dir.join("dir_b").join("Dup.wav");
        write_synthetic_wav(&a, 22_050, 0.25);
        write_synthetic_wav(&b, 22_050, 0.25);

        Library::update(&ws.library_path, |lib| {
            lib.add_files(&ws.music_dir, true, None).map(|_| ())
        })
        .unwrap();

        // Remove both originals and place one copy in a new folder -> ambiguous (2 missing, 1 candidate).
        fs::remove_file(&a).unwrap();
        fs::remove_file(&b).unwrap();
        let moved = ws.music_dir.join("dir_c").join("Dup.wav");
        write_synthetic_wav(&moved, 22_050, 0.25);

        let summary = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            None,
        )
        .unwrap();
        assert_eq!(summary.relinked_count, 0);
        assert_eq!(summary.added_count, 1);
        assert_eq!(summary.removed_count, 2);
        assert_eq!(summary.unchanged_count, 0);
    }

    #[test]
    fn update_workspace_handles_empty_workspace_corrupt_files_and_missing_download_dest() {
        let ws = WorkspaceFixture::new("edge-cases");

        // Empty workspace succeeds with zero counts.
        let empty_summary = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            None,
        )
        .unwrap();
        assert_eq!(empty_summary, UpdateSummary::default());

        // Add a valid track and a track under a download_dest that later disappears (e.g. unmounted drive).
        let dl_dest = ws.root.join("external_dl");
        fs::create_dir_all(&dl_dest).unwrap();
        let dl_track = dl_dest.join("MountedTrack.wav");
        write_synthetic_wav(&dl_track, 22_050, 0.2);

        let valid_track = ws.music_dir.join("Healthy.wav");
        write_synthetic_wav(&valid_track, 22_050, 0.2);
        let corrupt_track = ws.music_dir.join("Corrupt.wav");
        fs::write(&corrupt_track, b"not a valid audio file").unwrap();

        let first = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            Some(&dl_dest),
        )
        .unwrap();
        assert_eq!(first.added_count, 2);
        assert_eq!(first.skipped_files.len(), 1);
        assert!(first.skipped_files[0].contains("Corrupt.wav"));

        // Simulate unmounted download_dest: directory no longer exists.
        fs::remove_dir_all(&dl_dest).unwrap();

        let after_unmount = Library::update_workspace(
            &ws.library_path,
            &ws.working_dir,
            &ws.config_dir,
            &ws.playlists_dir,
            Some(&dl_dest),
        )
        .unwrap();
        assert_eq!(after_unmount.removed_count, 0);
        assert_eq!(after_unmount.unchanged_count, 2);
        assert!(
            after_unmount
                .skipped_files
                .iter()
                .any(|w| w.contains("does not exist") || w.contains("skipping scan")),
            "expected missing download_dest warning, got {:?}",
            after_unmount.skipped_files
        );

        let lib = Library::load(&ws.library_path).unwrap();
        assert_eq!(lib.songs.len(), 2);
        assert!(lib.songs.iter().any(|s| s.title == "MountedTrack"));
    }
}
