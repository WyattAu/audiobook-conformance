# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) versions follow [semver](https://semver.org/).

### Added

- **Three MP3 chapter fixtures**, the shape this harness covered least. `CHAP` existed in
  `id3-core` and `audiobook-core` read it, but every MP3 in the corpus was ffmpeg-written,
  which is a single writer with a single set of habits:
  - `CHAP` frames with **no** `CTOC`, which the spec permits and ffmpeg cannot produce.
  - A `CTOC` whose flags are `0x02` \u{2014} top level, not ordered. Every ffmpeg-written
    TOC sets *both* flag bits (`0x03`), so a reader that transposes them reads every
    corpus file correctly by accident. This is the byte mutagen writes for
    `CTOCFlags.TOP_LEVEL`, and it is the fixture that would have caught id3-core's
    transposed flags.
  - An **ID3v2.3** tag, which ffmpeg never writes. Its frame header is ten bytes in
    practice, its sizes are plain rather than synch-safe, and UTF-8 is not a legal text
    encoding in it at all.

- **`Corpus::generate` now asserts each fixture contains what its recipe promised**
  (`FileKind::expected_chapters`). A fixture that failed to encode its chapters was the
  most dangerous failure available here: both sides then found nothing and agreed, so
  every downstream comparison passed while testing nothing. It is not hypothetical \u{2014} a
  `CHAP` fixture that lost its sub-frame header reported a clean pass for a whole
  iteration. `Error::FixtureFailed` stops the run, and a sabotaged fixture was confirmed
  to exit non-zero.

- **`UNCHECKED_FIELDS`** \u{2014; a printed list of what is *not* compared, so a run cannot
  imply coverage it lacks: ID3 chapter end times, `CTOC` hierarchy and depth, `CHAP` byte
  offsets, and the specification-shaped v2.3 header that no available tool reads.

### Fixed

- **The harness's own JSON reader did not unescape strings.** Its doc said "unescaped"
  while the code returned raw text, so any title containing a quote, a backslash or a
  newline compared as a disagreement every time. Found the moment a corpus title contained
  a newline. Full JSON escape support now, including `\uXXXX` and surrogate pairs, and an
  escape JSON does not define is an error rather than a guess.

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

### Added

- **Symphonia is wired in as a third oracle.** It was declared and named in `Source` but
  never called, which is worse than absent: a capability a harness lists is a capability a
  reader assumes it has. With only ffprobe, every finding is "the estate disagrees with
  ffmpeg", and agreement has a blind spot — there is no way to tell agreement from a shared
  misreading. Symphonia is pure Rust with no shared code or history with either, so it
  breaks ties. It also runs where ffmpeg is not installed.

  Scope, stated plainly: Symphonia provides **container identification**, comparing
  `audiobook-core::container::detect` against a probe that shares nothing with it. It does
  *not* provide duration, despite this module being written for that first. Symphonia 0.6
  exposes no duration without decoding — `Track`'s codec parameters carry a sample rate and
  a codec id but no frame count, and neither the MP3 nor the ISOMP4 reader populates one —
  so the first version returned `None` for every file, which would have made a duration
  comparison pass vacuously with both sides reporting nothing. Duration remains ffprobe's,
  and the reason is written in the module so nobody re-derives it.

  Three Symphonia API facts cost real time and are recorded where the next person will
  look: the probe lives at `symphonia::core::formats::probe`, not `core::probe`; a bare
  `Probe` knows no formats, so `symphonia::default::register_enabled_formats` must be
  called; and the method is `probe`, not `format`, taking its options by value.

### Reported, not fixed here

- **ffmpeg cannot read a chapter title ending in a backslash.** Escaped correctly, its
  reader takes the escaped pair as a line continuation, swallows the next `[CHAPTER]`
  header, and produces a chapter count that is short by one with a start time of zero -
  with nothing on stderr. Three chapters in, two out. Attributed to ffmpeg and reported on
  every run; `audiobook_core::sidecar::ffmpeg_reads_back` exists so a caller can warn
  before writing such a file.
