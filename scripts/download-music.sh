#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/termusic"
DEST_CONFIG_FILE="$CONFIG_DIR/download-dest"

FORMAT="mp3"
DEST_ARG=""
HAS_DEST_ARG=0
SET_DEST_ARG=""
HAS_SET_DEST_ARG=0
PLAYLIST_MODE=0
EMBED_THUMBNAIL=1
URLS=()

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME [OPTIONS] [--] <URL...>
       $SCRIPT_NAME --set-dest <DIR> [OPTIONS] [URL...]

Download high-quality audio with embedded metadata using yt-dlp.

Options:
  -d, --dest <DIR>        Download destination directory for this run
      --set-dest <DIR>    Persist default destination directory to:
                          $DEST_CONFIG_FILE
  -f, --format <FORMAT>   Target audio format: mp3 (default), flac, m4a, ogg, wav, aac
      --playlist          Allow downloading playlists (default: single track / --no-playlist)
      --no-embed-thumbnail
                          Skip embedding cover art thumbnail
  -h, --help              Show this help message and exit

Environment:
  TERMUSIC_DEST           Fallback destination directory if -d/--dest is not passed
  XDG_CONFIG_HOME         Base config directory for saved download-dest

Examples:
  $SCRIPT_NAME --set-dest ~/Music/termusic-workspace/music
  $SCRIPT_NAME "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
  $SCRIPT_NAME -d /tmp/songs -f flac "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_value() {
    local flag="$1"
    local argc="$2"
    if [[ "$argc" -lt 2 ]]; then
        die "option '$flag' requires an argument"
    fi
}

trim() {
    local s="$1"
    s="${s#"${s%%[![:space:]]*}"}"
    s="${s%"${s##*[![:space:]]}"}"
    printf '%s' "$s"
}

resolve_abs_dir() {
    local dir="$1"
    if [[ -z "$dir" ]]; then
        die "directory path cannot be empty"
    fi
    mkdir -p -- "$dir"
    (cd -- "$dir" && pwd -P)
}

save_default_dest() {
    local raw_dir="$1"
    local abs_dir
    abs_dir="$(resolve_abs_dir "$raw_dir")"
    mkdir -p -- "$CONFIG_DIR"
    local tmp_file="$DEST_CONFIG_FILE.tmp.$$"
    printf '%s\n' "$abs_dir" > "$tmp_file"
    mv -f -- "$tmp_file" "$DEST_CONFIG_FILE"
    printf 'Saved default download destination: %s\n' "$abs_dir"
}

read_saved_dest() {
    if [[ -f "$DEST_CONFIG_FILE" ]]; then
        local first_line=""
        IFS= read -r first_line < "$DEST_CONFIG_FILE" || true
        first_line="$(trim "$first_line")"
        if [[ -n "$first_line" ]]; then
            printf '%s' "$first_line"
            return 0
        fi
    fi
    return 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        -d|--dest)
            require_value "$1" "$#"
            DEST_ARG="$2"
            HAS_DEST_ARG=1
            shift 2
            ;;
        --dest=*)
            DEST_ARG="${1#*=}"
            HAS_DEST_ARG=1
            shift
            ;;
        --set-dest)
            require_value "$1" "$#"
            SET_DEST_ARG="$2"
            HAS_SET_DEST_ARG=1
            shift 2
            ;;
        --set-dest=*)
            SET_DEST_ARG="${1#*=}"
            HAS_SET_DEST_ARG=1
            shift
            ;;
        -f|--format)
            require_value "$1" "$#"
            FORMAT="$2"
            shift 2
            ;;
        --format=*)
            FORMAT="${1#*=}"
            shift
            ;;
        --playlist)
            PLAYLIST_MODE=1
            shift
            ;;
        --no-embed-thumbnail)
            EMBED_THUMBNAIL=0
            shift
            ;;
        --)
            shift
            while [[ $# -gt 0 ]]; do
                URLS+=("$1")
                shift
            done
            ;;
        -*)
            die "unknown option: $1 (run with --help for usage)"
            ;;
        *)
            URLS+=("$1")
            shift
            ;;
    esac
done

FORMAT="$(printf '%s' "$FORMAT" | tr '[:upper:]' '[:lower:]')"
case "$FORMAT" in
    mp3|flac|m4a|ogg|wav|aac) ;;
    *)
        die "unsupported audio format '$FORMAT' (expected: mp3, flac, m4a, ogg, wav, aac)"
        ;;
esac

if [[ "$HAS_SET_DEST_ARG" -eq 1 ]]; then
    if [[ -z "$(trim "$SET_DEST_ARG")" ]]; then
        die "--set-dest requires a non-empty directory path"
    fi
    save_default_dest "$SET_DEST_ARG"
    if [[ ${#URLS[@]} -eq 0 ]]; then
        exit 0
    fi
fi

if [[ ${#URLS[@]} -eq 0 ]]; then
    die "missing URL (pass <URL...> or --set-dest <DIR>; run with --help for usage)"
fi

DEST_DIR=""
if [[ "$HAS_DEST_ARG" -eq 1 ]]; then
    if [[ -z "$(trim "$DEST_ARG")" ]]; then
        die "--dest requires a non-empty directory path"
    fi
    DEST_DIR="$DEST_ARG"
elif [[ "$HAS_SET_DEST_ARG" -eq 1 ]]; then
    DEST_DIR="$SET_DEST_ARG"
elif [[ -n "${TERMUSIC_DEST:-}" ]]; then
    DEST_DIR="$(trim "$TERMUSIC_DEST")"
elif saved_dest="$(read_saved_dest)"; then
    DEST_DIR="$saved_dest"
fi

if [[ -z "$DEST_DIR" ]]; then
    die "download destination is not set. Pass -d <dir> / --dest <dir>, set TERMUSIC_DEST, or run --set-dest <dir>."
fi

DEST_DIR="$(resolve_abs_dir "$DEST_DIR")"

YTDLP_BIN=""
if [[ -x "$HOME/.local/bin/yt-dlp" ]]; then
    YTDLP_BIN="$HOME/.local/bin/yt-dlp"
elif command -v yt-dlp >/dev/null 2>&1; then
    YTDLP_BIN="$(command -v yt-dlp)"
else
    die "yt-dlp not found (checked $HOME/.local/bin/yt-dlp and PATH)"
fi

if ! command -v ffmpeg >/dev/null 2>&1 && ! command -v avconv >/dev/null 2>&1; then
    die "ffmpeg is required for audio extraction (-x) but was not found on PATH"
fi

YT_FORMAT="$FORMAT"
if [[ "$FORMAT" == "ogg" ]]; then
    YT_FORMAT="vorbis"
fi

CMD=(
    "$YTDLP_BIN"
    -f "bestaudio/best"
    -x
    --audio-format "$YT_FORMAT"
    --audio-quality 0
    --add-metadata
    -P "$DEST_DIR"
    -o "%(title)s.%(ext)s"
)

if [[ "$PLAYLIST_MODE" -eq 1 ]]; then
    CMD+=(--yes-playlist)
else
    CMD+=(--no-playlist)
fi

if command -v deno >/dev/null 2>&1; then
    CMD+=(--js-runtimes "deno:$(command -v deno)")
elif command -v node >/dev/null 2>&1; then
    CMD+=(--js-runtimes "node:$(command -v node)")
elif [[ -x "/mnt/c/Users/patri/scoop/apps/nodejs/current/node.exe" ]]; then
    CMD+=(--js-runtimes "node:/mnt/c/Users/patri/scoop/apps/nodejs/current/node.exe")
fi

USE_EMBED_THUMB=0
if [[ "$EMBED_THUMBNAIL" -eq 1 ]]; then
    case "$FORMAT" in
        wav|aac)
            printf 'note: thumbnail embedding is not supported for %s; skipping --embed-thumbnail\n' "$FORMAT" >&2
            ;;
        ogg)
            if python3 -c "import mutagen" >/dev/null 2>&1; then
                USE_EMBED_THUMB=1
            else
                printf 'note: python3 mutagen not found for ogg cover art; skipping --embed-thumbnail\n' >&2
            fi
            ;;
        *)
            USE_EMBED_THUMB=1
            ;;
    esac
fi

if [[ "$USE_EMBED_THUMB" -eq 1 ]]; then
    CMD+=(--embed-thumbnail)
fi

printf 'Downloading (%s) into: %s\n' "$FORMAT" "$DEST_DIR"
exec "${CMD[@]}" -- "${URLS[@]}"
