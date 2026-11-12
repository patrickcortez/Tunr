use std::ffi::{OsStr, OsString};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::error::{Result, TermusicError};

/// Environment snapshot for discovering external tools (`yt-dlp`, `ffmpeg`, `deno`, `node`, etc.)
/// without mutating process-wide environment variables in tests.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolEnv {
    pub home_dir: Option<PathBuf>,
    pub path_var: Option<OsString>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub pathext: Option<OsString>,
    pub extra_search_dirs: Vec<PathBuf>,
}

impl ToolEnv {
    /// Capture the current process environment and platform-specific fallback tool directories.
    pub(crate) fn from_system() -> Self {
        #[cfg_attr(not(any(target_os = "macos", windows)), allow(unused_mut))]
        let mut extra_search_dirs = Vec::new();
        #[cfg(target_os = "macos")]
        {
            extra_search_dirs.push(PathBuf::from("/opt/homebrew/bin"));
            extra_search_dirs.push(PathBuf::from("/usr/local/bin"));
        }
        #[cfg(windows)]
        {
            if let Some(local_app_data) = dirs::data_local_dir() {
                extra_search_dirs.push(local_app_data.join("Microsoft").join("WinGet").join("Links"));
            }
            extra_search_dirs.push(PathBuf::from(r"C:\ProgramData\chocolatey\bin"));
        }

        Self {
            home_dir: std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(dirs::home_dir),
            path_var: std::env::var_os("PATH"),
            pathext: std::env::var_os("PATHEXT"),
            extra_search_dirs,
        }
    }
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .map(|meta| meta.is_file() && (meta.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false)
    }
}

#[cfg(windows)]
fn windows_executable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    const FALLBACK_EXTS: &[&str] = &[".exe", ".cmd", ".bat", ".com"];
    let mut parsed = Vec::new();
    if let Some(raw) = pathext {
        for part in raw.to_string_lossy().split(';') {
            let trimmed = part.trim().to_ascii_lowercase();
            if matches!(trimmed.as_str(), ".exe" | ".cmd" | ".bat" | ".com")
                && !parsed.contains(&trimmed)
            {
                parsed.push(trimmed);
            }
        }
    }
    if parsed.is_empty() {
        FALLBACK_EXTS.iter().map(|s| (*s).to_string()).collect()
    } else {
        // Ensure `.exe` takes precedence over `.cmd`/`.bat` shims if both exist.
        if let Some(exe_pos) = parsed.iter().position(|ext| ext == ".exe")
            && exe_pos > 0
        {
            let exe = parsed.remove(exe_pos);
            parsed.insert(0, exe);
        }
        parsed
    }
}

/// Search `dir` for an executable matching `binary` (respecting `PATHEXT` on Windows).
pub(crate) fn find_in_dir(dir: &Path, binary: &str, pathext: Option<&OsStr>) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if Path::new(binary).extension().is_some() {
            let candidate = dir.join(binary);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
            return None;
        }
        for ext in windows_executable_extensions(pathext) {
            let candidate = dir.join(format!("{binary}{ext}"));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = pathext;
        let candidate = dir.join(binary);
        is_executable_file(&candidate).then_some(candidate)
    }
}

fn find_on_path_with_pathext(
    binary: &str,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var?) {
        if let Some(found) = find_in_dir(&dir, binary, pathext) {
            return Some(found);
        }
    }
    None
}

/// Locate `binary_name` on `PATH`, Scoop shims (Windows), or `env.extra_search_dirs`.
pub(crate) fn find_in_env(binary_name: &str, env: &ToolEnv) -> Option<PathBuf> {
    if let Some(found) =
        find_on_path_with_pathext(binary_name, env.path_var.as_deref(), env.pathext.as_deref())
    {
        return Some(found);
    }

    #[cfg(windows)]
    if let Some(home) = env.home_dir.as_deref() {
        let scoop_shims = home.join("scoop").join("shims");
        if let Some(found) = find_in_dir(&scoop_shims, binary_name, env.pathext.as_deref()) {
            return Some(found);
        }
    }

    for dir in &env.extra_search_dirs {
        if let Some(found) = find_in_dir(dir, binary_name, env.pathext.as_deref()) {
            return Some(found);
        }
    }
    None
}

#[cfg(any(windows, test))]
fn append_windows_quoted_arg(out: &mut OsString, arg: &OsStr) {
    let text = arg.to_string_lossy();
    let needs_quotes = text.is_empty()
        || text.chars().any(|ch| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '"' | '&' | '|' | '<' | '>' | '(' | ')' | '^' | ',' | ';' | '=' | '%' | '!'
                )
        });
    if !needs_quotes {
        out.push(arg);
        return;
    }

    out.push("\"");
    let mut backslashes = 0usize;
    for ch in text.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push("\\");
                }
                backslashes = 0;
                out.push("\"");
            }
            _ => {
                for _ in 0..backslashes {
                    out.push("\\");
                }
                backslashes = 0;
                let mut buf = [0u8; 4];
                out.push(ch.encode_utf8(&mut buf));
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push("\\");
    }
    out.push("\"");
}

/// Format a `.cmd`/`.bat` invocation wrapped in outer `"..."` for `cmd.exe /D /S /C`.
#[cfg(any(windows, test))]
pub(crate) fn format_windows_cmd_batch_line(binary: &Path, args: &[OsString]) -> OsString {
    let mut line = OsString::from("\"");
    append_windows_quoted_arg(&mut line, binary.as_os_str());
    for arg in args {
        line.push(" ");
        append_windows_quoted_arg(&mut line, arg);
    }
    line.push("\"");
    line
}

/// Build a [`tokio::process::Command`] for `binary`, handling Windows `.cmd`/`.bat` shims and
/// suppressing console windows (`CREATE_NO_WINDOW`).
pub(crate) fn command_for_binary(binary: &Path, args: &[OsString]) -> Command {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let is_batch = binary
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
        let mut cmd = if is_batch {
            let mut c = Command::new("cmd");
            c.arg("/D").arg("/S").arg("/C");
            c.raw_arg(format_windows_cmd_batch_line(binary, args));
            c
        } else {
            let mut c = Command::new(binary);
            c.args(args);
            c
        };
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(binary);
        cmd.args(args);
        cmd
    }
}

/// Locate `yt-dlp` in `~/.local/bin`, `PATH`, or platform package manager directories.
pub(crate) fn find_yt_dlp(env: &ToolEnv) -> Result<PathBuf> {
    if let Some(home) = env.home_dir.as_deref() {
        let local_bin_dir = home.join(".local").join("bin");
        if let Some(found) = find_in_dir(&local_bin_dir, "yt-dlp", env.pathext.as_deref()) {
            return Ok(found);
        }
    }

    find_in_env("yt-dlp", env).ok_or_else(|| {
        #[cfg(target_os = "android")]
        let hint = "yt-dlp not found in ~/.local/bin or PATH (install via: pkg install yt-dlp)";
        #[cfg(target_os = "macos")]
        let hint = "yt-dlp not found in ~/.local/bin, PATH, or Homebrew (install via: brew install yt-dlp)";
        #[cfg(windows)]
        let hint = "yt-dlp not found in ~/.local/bin, PATH, or WinGet/Scoop (install via: winget install yt-dlp.yt-dlp)";
        #[cfg(not(any(target_os = "android", target_os = "macos", windows)))]
        let hint = "yt-dlp not found in ~/.local/bin or PATH";

        TermusicError::NotFound(hint.into())
    })
}

/// Locate `ffmpeg` (or fallback `avconv`) in `env`.
pub(crate) fn find_ffmpeg(env: &ToolEnv) -> Option<PathBuf> {
    find_in_env("ffmpeg", env).or_else(|| find_in_env("avconv", env))
}

/// Detect an available JavaScript runtime (`deno` preferred, then `node`) for `yt-dlp --js-runtimes`.
pub(crate) fn detect_js_runtime(env: &ToolEnv) -> Option<String> {
    if let Some(deno_path) = find_in_env("deno", env) {
        Some(format!("deno:{}", deno_path.display()))
    } else if let Some(node_path) = find_in_env("node", env) {
        Some(format!("node:{}", node_path.display()))
    } else {
        None
    }
}

/// Pick the most informative diagnostic line from captured stderr text (last `ERROR:` line, or last non-empty line).
pub(crate) fn extract_diagnostic_line(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|line| line.starts_with("ERROR:"))
        .or_else(|| lines.last())
        .map(|line| (*line).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termusic-ytdlp-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_exec(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn find_yt_dlp_prefers_local_bin_and_detects_js_runtime() {
        let root = temp_dir("discover");
        let home = root.join("home");
        let path_dir = root.join("bin");
        let extra_dir = root.join("extra");

        let empty_env = ToolEnv {
            home_dir: Some(home.clone()),
            path_var: Some(OsString::from(path_dir.as_os_str())),
            pathext: None,
            extra_search_dirs: vec![extra_dir.clone()],
        };
        assert!(matches!(find_yt_dlp(&empty_env), Err(TermusicError::NotFound(_))));
        assert_eq!(find_ffmpeg(&empty_env), None);
        assert_eq!(detect_js_runtime(&empty_env), None);

        #[cfg(windows)]
        let (yt_name, node_name, deno_name) = ("yt-dlp.exe", "node.exe", "deno.exe");
        #[cfg(not(windows))]
        let (yt_name, node_name, deno_name) = ("yt-dlp", "node", "deno");

        let path_yt = write_exec(&path_dir, yt_name);
        assert_eq!(find_yt_dlp(&empty_env).unwrap(), path_yt);

        let local_yt = write_exec(&home.join(".local").join("bin"), yt_name);
        assert_eq!(find_yt_dlp(&empty_env).unwrap(), local_yt);

        let node_bin = write_exec(&extra_dir, node_name);
        assert_eq!(
            detect_js_runtime(&empty_env).as_deref(),
            Some(format!("node:{}", node_bin.display())).as_deref()
        );

        let deno_bin = write_exec(&path_dir, deno_name);
        assert_eq!(
            detect_js_runtime(&empty_env).as_deref(),
            Some(format!("deno:{}", deno_bin.display())).as_deref()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_diagnostic_prefers_last_error_line() {
        let stderr = "WARNING: something\nERROR: first failure\nERROR: real cause\ntrailing note\n";
        assert_eq!(
            extract_diagnostic_line(stderr).as_deref(),
            Some("ERROR: real cause")
        );
        assert_eq!(
            extract_diagnostic_line("WARNING: a\n\n  plain stderr  \n").as_deref(),
            Some("plain stderr")
        );
        assert_eq!(extract_diagnostic_line("   \n\n"), None);
    }

    #[test]
    fn format_windows_cmd_batch_line_wraps_spaced_binary_and_args() {
        let bin = Path::new(r"C:\Program Files\yt-dlp\yt-dlp.cmd");
        let args = vec![
            OsString::from("--flat-playlist"),
            OsString::from("ytsearch5:lofi beats"),
        ];
        let rendered = format_windows_cmd_batch_line(bin, &args).to_string_lossy().into_owned();
        assert!(rendered.starts_with('"'));
        assert!(rendered.ends_with('"'));
        assert!(rendered.contains(r#""C:\Program Files\yt-dlp\yt-dlp.cmd""#));
        assert!(rendered.contains(r#""ytsearch5:lofi beats""#));
    }
}
