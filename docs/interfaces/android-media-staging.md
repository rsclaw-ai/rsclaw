# Android media staging

## Purpose

Android plugins receive local macOS file paths, but Android's clipboard host
primitive carries text only. `android-stage-file` bridges that gap by copying
one explicitly requested local file into a controlled device directory before
the plugin uses the target application's normal attachment picker.

## WIT contract

```wit
android-stage-file: func(local-path: string, media-kind: string) -> result<string, string>;
```

`media-kind` is one of `image`, `file`, or `audio`. The successful value is an
absolute device path. The host validates that the source is a regular local
file, stages it under `/sdcard/Download/rsclaw/`, uses a collision-resistant
filename, and returns the staged path. Images are also indexed by Android's
media scanner so WeChat's album picker can show them.

## Safety and failure behavior

- The primitive never runs a caller-supplied shell command.
- It rejects unknown media kinds, non-files, and paths whose basename is empty.
- A plugin must not claim delivery merely because staging succeeded; it must
  still use the target UI and verify the resulting outgoing attachment bubble.
- Staging does not grant a plugin permission to inspect any unrelated local or
  device files.
