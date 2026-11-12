use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::audio::{is_supported_extension, probe_track};
use crate::cli::validate_add_path;
use crate::error::{Result, TermusicError};
use crate::ipc::TargetKind;
use crate::paths::{lock_path_for, with_file_lock, write_json_atomic};
use crate::platform::canonicalize_clean;

pub const LIBRARY_VERSION: u32 = 1;

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

/// Outcome returned by `Library::delete_target`.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteOutcome {
    Song(Song),
    Playlist(Playlist),
}

/// Resolved playback target and ordered queue ready for `IpcRequest::Play`.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    Playlist {
        playlist: Playlist,
        songs: Vec<Song>,
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
            PlayTarget::Playlist { playlist, songs } => (songs, None, Some(playlist.name)),
            PlayTarget::Song { queue, .. } => (queue, Some(0), None),
        }
    }
}

enum LookupMatch {
    None,
    One(usize),
    Multiple(Vec<String>),
}

impl Library {
    fn load_unlocked(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let lib: Library = serde_json::from_str(&raw)?;
        Ok(lib)
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

    /// Add a file or recursive folder (`folder = true`) to the library index,
    /// optionally appending the added/existing songs to `playlist_name`.
    pub fn add_files(
        &mut self,
        path: &Path,
        folder: bool,
        playlist_name: Option<&str>,
    ) -> Result<AddSummary> {
        validate_add_path(path, folder)?;

        let clean_playlist = match playlist_name {
            Some(name) => {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    return Err(TermusicError::InvalidArg(
                        "playlist name cannot be empty".to_string(),
                    ));
                }
                Some(trimmed.to_string())
            }
            None => None,
        };

        let (candidates, walk_skipped) = collect_candidate_files(path, folder)?;
        let mut summary = AddSummary {
            playlist_name: clean_playlist.clone(),
            skipped_files: walk_skipped,
            ..AddSummary::default()
        };
        let mut resolved_ids = Vec::with_capacity(candidates.len());
        let mut last_probe_err: Option<TermusicError> = None;

        for canonical in candidates {
            if let Some(existing) = self.songs.iter().find(|s| s.path == canonical) {
                summary.existing_count += 1;
                resolved_ids.push(existing.id.clone());
                continue;
            }

            match probe_track(&canonical) {
                Ok(probe) => {
                    let title = probe
                        .title
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| fallback_title(&canonical));
                    let artist = clean_opt(probe.artist);
                    let album = clean_opt(probe.album);
                    let song = Song {
                        id: Uuid::new_v4().to_string(),
                        title: title.clone(),
                        artist,
                        album,
                        path: canonical,
                        duration_secs: probe.duration_secs,
                    };
                    resolved_ids.push(song.id.clone());
                    summary.added_titles.push(title);
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
            let pl_idx = match self.lookup_playlist(&pl_name) {
                LookupMatch::One(idx) => idx,
                LookupMatch::Multiple(names) => {
                    return Err(TermusicError::Ambiguous(format!(
                        "playlist '{pl_name}' matched multiple entries: {}",
                        names.join(", ")
                    )));
                }
                LookupMatch::None => {
                    self.playlists.push(Playlist {
                        name: pl_name.clone(),
                        song_ids: Vec::new(),
                    });
                    self.playlists.len() - 1
                }
            };

            let pl = &mut self.playlists[pl_idx];
            summary.playlist_name = Some(pl.name.clone());
            for song_id in resolved_ids {
                if !pl.song_ids.contains(&song_id) {
                    pl.song_ids.push(song_id);
                    summary.playlist_added_count += 1;
                }
            }
        }

        Ok(summary)
    }

    /// Create a new playlist from comma-separated existing song titles.
    /// Fails atomically if the playlist already exists or any song title is missing or ambiguous.
    pub fn create_playlist(&mut self, name: &str, song_titles: &[String]) -> Result<Playlist> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err(TermusicError::InvalidArg(
                "playlist name cannot be empty".to_string(),
            ));
        }
        if song_titles.is_empty() {
            return Err(TermusicError::InvalidArg(
                "playlist must contain at least one song".to_string(),
            ));
        }

        match self.lookup_playlist(trimmed_name) {
            LookupMatch::One(idx) => {
                return Err(TermusicError::AlreadyExists(format!(
                    "playlist '{}' already exists",
                    self.playlists[idx].name
                )));
            }
            LookupMatch::Multiple(_) => {
                return Err(TermusicError::AlreadyExists(format!(
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
            match self.lookup_song(query) {
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
                return Err(TermusicError::Ambiguous(msg));
            }
            return Err(TermusicError::NotFound(msg));
        }

        if resolved_ids.is_empty() {
            return Err(TermusicError::InvalidArg(
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

    /// Delete a song or playlist from the library index WITHOUT deleting audio files from disk.
    /// Deleting a song strips its ID from every playlist.
    pub fn delete_target(&mut self, query: &str, kind: TargetKind) -> Result<DeleteOutcome> {
        let q = query.trim();
        if q.is_empty() {
            return Err(TermusicError::InvalidArg(
                "delete target cannot be empty".to_string(),
            ));
        }

        match kind {
            TargetKind::Song => self.delete_song_by_query(q),
            TargetKind::Playlist => self.delete_playlist_by_query(q),
            TargetKind::Auto => {
                let song_match = self.lookup_song(q);
                let playlist_match = self.lookup_playlist(q);

                let has_song = !matches!(song_match, LookupMatch::None);
                let has_playlist = !matches!(playlist_match, LookupMatch::None);

                if has_song && has_playlist {
                    return Err(TermusicError::Ambiguous(format!(
                        "'{q}' matches both a song and a playlist; specify `--song` or `--playlist`"
                    )));
                }
                if has_song {
                    self.delete_song_by_query(q)
                } else if has_playlist {
                    self.delete_playlist_by_query(q)
                } else {
                    Err(TermusicError::NotFound(format!(
                        "no song or playlist found matching '{q}'"
                    )))
                }
            }
        }
    }

    /// Resolve a song title or playlist name into a playable queue.
    /// When resolving a song, the library songs are rotated so the selected song starts at index 0
    /// and `next`/`prev` navigate through the rest of the library.
    pub fn resolve_play_target(&self, query: &str, kind: TargetKind) -> Result<PlayTarget> {
        let q = query.trim();
        if q.is_empty() {
            return Err(TermusicError::InvalidArg(
                "play target cannot be empty".to_string(),
            ));
        }

        match kind {
            TargetKind::Song => self.resolve_song_play_target(q),
            TargetKind::Playlist => self.resolve_playlist_play_target(q),
            TargetKind::Auto => {
                let song_match = self.lookup_song(q);
                let playlist_match = self.lookup_playlist(q);

                let has_song = !matches!(song_match, LookupMatch::None);
                let has_playlist = !matches!(playlist_match, LookupMatch::None);

                if has_song && has_playlist {
                    return Err(TermusicError::Ambiguous(format!(
                        "'{q}' matches both a song and a playlist; specify `--song` or `--playlist`"
                    )));
                }
                if has_song {
                    self.resolve_song_play_target(q)
                } else if has_playlist {
                    self.resolve_playlist_play_target(q)
                } else {
                    Err(TermusicError::NotFound(format!(
                        "no song or playlist found matching '{q}'"
                    )))
                }
            }
        }
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

    fn resolve_song_play_target(&self, query: &str) -> Result<PlayTarget> {
        let idx = self.resolve_unique_song_index(query)?;
        let selected = self.songs[idx].clone();
        let mut queue = Vec::with_capacity(self.songs.len());
        queue.extend_from_slice(&self.songs[idx..]);
        queue.extend_from_slice(&self.songs[..idx]);
        Ok(PlayTarget::Song { selected, queue })
    }

    fn resolve_playlist_play_target(&self, query: &str) -> Result<PlayTarget> {
        let idx = self.resolve_unique_playlist_index(query)?;
        let playlist = self.playlists[idx].clone();
        let songs = self.songs_for_playlist(&playlist);
        if songs.is_empty() {
            return Err(TermusicError::NotFound(format!(
                "playlist '{}' contains no playable songs",
                playlist.name
            )));
        }
        Ok(PlayTarget::Playlist { playlist, songs })
    }

    fn resolve_unique_song_index(&self, query: &str) -> Result<usize> {
        match self.lookup_song(query) {
            LookupMatch::One(idx) => Ok(idx),
            LookupMatch::None => Err(TermusicError::NotFound(format!(
                "no song found matching '{query}'"
            ))),
            LookupMatch::Multiple(matches) => Err(TermusicError::Ambiguous(format!(
                "multiple songs match '{query}': {}",
                matches.join(", ")
            ))),
        }
    }

    fn resolve_unique_playlist_index(&self, query: &str) -> Result<usize> {
        match self.lookup_playlist(query) {
            LookupMatch::One(idx) => Ok(idx),
            LookupMatch::None => Err(TermusicError::NotFound(format!(
                "no playlist found matching '{query}'"
            ))),
            LookupMatch::Multiple(matches) => Err(TermusicError::Ambiguous(format!(
                "multiple playlists match '{query}': {}",
                matches.join(", ")
            ))),
        }
    }

    fn lookup_song(&self, query: &str) -> LookupMatch {
        let exact: Vec<usize> = self
            .songs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.title == query)
            .map(|(i, _)| i)
            .collect();
        if !exact.is_empty() {
            return match exact.len() {
                1 => LookupMatch::One(exact[0]),
                _ => LookupMatch::Multiple(
                    exact
                        .into_iter()
                        .map(|i| format!("{} ({})", self.songs[i].title, self.songs[i].path.display()))
                        .collect(),
                ),
            };
        }

        let needle = query.to_lowercase();
        let ci: Vec<usize> = self
            .songs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.title.to_lowercase() == needle)
            .map(|(i, _)| i)
            .collect();
        match ci.len() {
            0 => LookupMatch::None,
            1 => LookupMatch::One(ci[0]),
            _ => LookupMatch::Multiple(
                ci.into_iter()
                    .map(|i| format!("{} ({})", self.songs[i].title, self.songs[i].path.display()))
                    .collect(),
            ),
        }
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

fn collect_candidate_files(path: &Path, folder: bool) -> Result<(Vec<PathBuf>, Vec<String>)> {
    if !folder {
        let canonical = canonicalize_clean(path)?;
        if !is_supported_extension(&canonical) {
            return Err(TermusicError::InvalidArg(format!(
                "unsupported audio file extension: {}",
                canonical.display()
            )));
        }
        return Ok((vec![canonical], Vec::new()));
    }

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                if err.depth() == 0 {
                    return Err(TermusicError::Io(std::io::Error::other(format!(
                        "failed walking {}: {err}",
                        path.display()
                    ))));
                }
                let entry_path = err
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| path.display().to_string());
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

    if files.is_empty() {
        return Err(TermusicError::NotFound(format!(
            "no supported audio files found in {}",
            path.display()
        )));
    }
    Ok((files, skipped))
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

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use super::*;

    fn temp_workspace(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-lib-test-{label}-{}",
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
        assert!(matches!(missing_err, TermusicError::NotFound(msg) if msg.contains("NoSuchSong")));
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
        assert!(matches!(dup_err, TermusicError::AlreadyExists(_)));

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
            Err(TermusicError::Ambiguous(_))
        ));
        assert!(matches!(
            lib.delete_target("Aurora", TargetKind::Auto),
            Err(TermusicError::Ambiguous(_))
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
            Err::<(), _>(TermusicError::InvalidArg("abort update".to_string()))
        })
        .unwrap_err();
        assert!(matches!(err, TermusicError::InvalidArg(_)));
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
        let missing = std::env::temp_dir().join(format!("termusic-missing-{}", Uuid::new_v4()));
        let err = collect_candidate_files(&missing, true).unwrap_err();
        assert!(matches!(err, TermusicError::Io(_)));
    }
}
