#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: encode-film.sh <film-slug> <frames-dir> <output-dir> [poster-frame] [--replace]

The frame directory must contain a contiguous 1280x720 JPEG sequence named
frame-0001.jpg, frame-0002.jpg, ... . The sequence is read at 8 fps and encoded
as a 24 fps H.264/yuv420p fast-start MP4. poster-frame defaults to 1.
EOF
}

if (( $# < 3 || $# > 5 )); then
  usage >&2
  exit 64
fi

film_slug="$1"
frames_dir="$2"
output_dir="$3"
poster_frame="${4:-1}"
replace_flag="${5:-}"

if [[ ! "$film_slug" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]]; then
  echo "Film slug must contain lowercase letters, digits, and single hyphens." >&2
  exit 64
fi
if [[ ! "$poster_frame" =~ ^[1-9][0-9]*$ ]]; then
  echo "Poster frame must be a positive integer." >&2
  exit 64
fi
if [[ -n "$replace_flag" && "$replace_flag" != "--replace" ]]; then
  usage >&2
  exit 64
fi
if [[ ! -d "$frames_dir" ]]; then
  echo "Frame directory does not exist: $frames_dir" >&2
  exit 66
fi

for dependency in ffmpeg ffprobe; do
  if ! command -v "$dependency" >/dev/null 2>&1; then
    echo "Missing required command: $dependency" >&2
    exit 69
  fi
done

shopt -s nullglob
film_frames=("$frames_dir"/frame-[0-9][0-9][0-9][0-9].jpg)
if (( ${#film_frames[@]} == 0 )); then
  echo "No frame-NNNN.jpg inputs found in $frames_dir" >&2
  exit 66
fi

for frame_index in "${!film_frames[@]}"; do
  printf -v expected_name 'frame-%04d.jpg' "$((frame_index + 1))"
  actual_name="${film_frames[$frame_index]##*/}"
  if [[ "$actual_name" != "$expected_name" ]]; then
    echo "Frame sequence is not contiguous: expected $expected_name, found $actual_name" >&2
    exit 65
  fi
  dimensions="$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=width,height -of csv=s=x:p=0 "${film_frames[$frame_index]}")"
  if [[ "$dimensions" != "1280x720" ]]; then
    echo "Frame must be 1280x720: ${film_frames[$frame_index]} is $dimensions" >&2
    exit 65
  fi
done

printf -v poster_name 'frame-%04d.jpg' "$poster_frame"
poster_source="$frames_dir/$poster_name"
if [[ ! -f "$poster_source" ]]; then
  echo "Poster source does not exist: $poster_source" >&2
  exit 66
fi

mkdir -p "$output_dir"
video_output="$output_dir/$film_slug.mp4"
poster_output="$output_dir/$film_slug.jpg"
if [[ "$replace_flag" != "--replace" && ( -e "$video_output" || -e "$poster_output" ) ]]; then
  echo "Output already exists; pass --replace to replace the matching pair." >&2
  exit 73
fi

temp_video="$output_dir/.$film_slug.$$.tmp.mp4"
temp_poster="$output_dir/.$film_slug.$$.tmp.jpg"
cleanup() {
  rm -f "$temp_video" "$temp_poster"
}
trap cleanup EXIT

ffmpeg -hide_banner -loglevel error \
  -framerate 8 -start_number 1 -i "$frames_dir/frame-%04d.jpg" \
  -vf 'scale=1280:720:in_range=pc:out_range=tv:flags=lanczos,format=yuv420p' \
  -c:v libx264 -preset slow -crf 22 -pix_fmt yuv420p \
  -color_range tv -r 24 -an -movflags +faststart -y "$temp_video"

cp "$poster_source" "$temp_poster"

video_codec="$(ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name -of default=nw=1:nk=1 "$temp_video")"
video_dimensions="$(ffprobe -v error -select_streams v:0 \
  -show_entries stream=width,height -of csv=s=x:p=0 "$temp_video")"
video_pixel_format="$(ffprobe -v error -select_streams v:0 \
  -show_entries stream=pix_fmt -of default=nw=1:nk=1 "$temp_video")"
video_rate="$(ffprobe -v error -select_streams v:0 \
  -show_entries stream=r_frame_rate -of default=nw=1:nk=1 "$temp_video")"
poster_codec="$(ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name -of default=nw=1:nk=1 "$temp_poster")"
poster_dimensions="$(ffprobe -v error -select_streams v:0 \
  -show_entries stream=width,height -of csv=s=x:p=0 "$temp_poster")"

if [[ "$video_codec" != "h264" || "$video_dimensions" != "1280x720" || \
      "$video_pixel_format" != "yuv420p" || "$video_rate" != "24/1" ]]; then
  echo "Encoded video failed contract: codec=$video_codec dimensions=$video_dimensions pixel_format=$video_pixel_format rate=$video_rate" >&2
  exit 65
fi
if [[ "$poster_codec" != "mjpeg" || "$poster_dimensions" != "1280x720" ]]; then
  echo "Encoded poster failed contract: codec=$poster_codec dimensions=$poster_dimensions" >&2
  exit 65
fi

mv -f "$temp_video" "$video_output"
mv -f "$temp_poster" "$poster_output"
trap - EXIT
printf 'Encoded %s\nPoster  %s\n' "$video_output" "$poster_output"
