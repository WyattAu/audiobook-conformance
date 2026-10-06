# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) versions follow [semver](https://semver.org/).

## [0.2.0] - 2026-10-06

The harness stops pointing every tool at files ffmpeg wrote.

### Added

- **A sidecar round trip with the roles reversed.** Every other check reads a file ffmpeg
  produced, which can only catch a *reader* defect: no byte this estate writes had ever
  been seen by an implementation that did not write it. That is the same blind spot that
  hid `mp4-core`’s `chpl` layout defect for several releases, when every `chpl` test had
  run against a box the crate wrote itself. So `sidecar::check` runs the format both ways
  round with ffmpeg as the independent implementation:
  - **write** - `audiobook_core::sidecar` renders a `;FFMETADATA1` file, ffmpeg muxes it,
    and ffprobe reports what survived;
  - **read** - ffmpeg dumps a real corpus file back out with `-f ffmetadata`, and the
    estate parses ffmpeg's own output.

  Ten awkward titles are exercised: `=`, `;`, `#`, a mid-title backslash, a newline,
  surrounding whitespace, one that looks like a section header, multi-byte UTF-8, quotes
  and percent signs, and an empty title.

- **`SidecarFinding::DocumentedFfmpegLimit`** - a divergence already attributed to a
  third party. Reported and counted, but not counted as a defect in this estate, and
  never presented as an agreement.

### Fixed

- **The harness's own JSON reader did not unescape strings.** Its doc said "unescaped"
  while the code returned the raw text, so a title containing a quote, a backslash or a
  newline was reported as a disagreement on every occurrence. Found the first time a
  corpus title contained a newline - that is, by the new sidecar check. Full JSON escape
  support now, including `\uXXXX` and surrogate pairs, and an escape JSON does not define
  is an error rather than a guess.

### Reported, not fixed here

- **ffmpeg cannot read a chapter title ending in a backslash.** Escaped correctly, its
  reader takes the escaped pair as a line continuation, swallows the next `[CHAPTER]`
  header, and produces a chapter count that is short by one with a start time of zero -
  with nothing on stderr. Three chapters in, two out. Attributed to ffmpeg and reported on
  every run; `audiobook_core::sidecar::ffmpeg_reads_back` exists so a caller can warn
  before writing such a file.
