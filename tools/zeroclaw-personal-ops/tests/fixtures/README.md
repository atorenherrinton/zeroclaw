# Synthetic MP4 fixture

`black.mp4` is one black 16-by-16 H.264 baseline frame, generated entirely from
FFmpeg's color source. It contains no captured media, personal data or messages.
Size: 1,440 bytes. SHA-256:
`5046ca6b29719950202ebe3d4cf9e15a9f3fd4642d2f999203e1b70bfe802c69`.

Generated with FFmpeg 9.0.1 (Homebrew 9.0.1_1), libx264, on macOS:

```sh
ffmpeg -nostdin -v error -f lavfi -i color=c=black:s=16x16:r=1 \
  -frames:v 1 -an -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
  -threads 1 -fflags +bitexact -flags:v +bitexact -map_metadata -1 \
  -movflags +faststart black.mp4
ffmpeg -nostdin -v error -xerror -i black.mp4 -f null -
ffprobe -v error -show_entries stream=codec_name,width,height,nb_frames \
  -of json black.mp4
```

The decode command passed; ffprobe reported `h264`, 16 by 16, one frame.
Encoding with another FFmpeg/libx264 build may change the bytes and hash.
The checked-in fixture is consumed by Rust tests without requiring FFmpeg.
Test mutations are intentionally invalid or unsupported envelopes; their
rejection is not a claim of full codec or playability validation.
