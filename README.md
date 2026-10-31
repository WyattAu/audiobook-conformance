# audiobook-conformance

Conformance harness for the WyattAu audiobook stack; generates a corpus with
**ffmpeg**, reads it back with `mp4-core` / `id3-core` / `audiobook-core`, and
reports every disagreement against **ffprobe**'s own reading of the same file.

**Layer:** L3 · **Consumes:** `mp4-core`, `id3-core`, `audiobook-core` ·
**Independent references:** ffmpeg/ffprobe, Symphonia

## Why this repo exists

The three crates it checks are written by the same person and therefore share blind
spots. A parser that agrees with itself proves nothing.

This one found a bug on its first run that months of unit tests had missed:
**`mp4-core` could not read a single real Nero chapter list.** Its `chpl` reader was
built from the Adobe FLV specification, which is wrong in two places; the chapter
count is one byte rather than four, and version 0 has no reserved bytes. Every test
that had ever passed on `chpl` had done so against a box the crate had written
itself.

## Usage

```sh
cargo run -- --root /tmp/audiobook-corpus
```

```
── corpus ──────────────────────────────────────────────────────
generating in /tmp/audiobook-corpus
6 files generated

── summary ────────────────────────────────────────────────────
✓ M4B with Nero chpl chapters 1/1 agree with ffprobe
✓ bare M4A 1/1 agree with ffprobe
✓ CBR MP3 with ID3v2 1/1 agree with ffprobe
✓ a folder of MP3s 3/3 agree with ffprobe

6 agree, 0 disagree, 0 carry an unchecked field
```

Exits non-zero when anything disagrees, so it drops straight into CI.

## What it generates

| Kind | Why |
|---|---|
| M4B with chapters, from an FFMetadata sidecar | ffmpeg writes **both** a Nero `chpl` and a QuickTime `tref`/`chap` chapter track; the two mechanisms are independent and real files carry one, the other, or both |
| Bare M4A | must report **no** chapters, so a reader that invents them is caught |
| CBR MP3 with ID3v2 | the common case, with `CHAP` frames |
| VBR MP3 | exercises the Xing/Info/VBRI path |
| Folder of three MP3s | the multi-file shape, where chapter offsets need per-file durations |

The source audio is a synthesised 440 Hz tone, generated rather than committed, so
the corpus is reproducible from source and the repository stays small.

## What it compares

- **Chapters**; count, start offset (2 ms tolerance) and title, against ffprobe.
- **Duration**; 50 ms tolerance, about one AAC frame. Tighter than nothing because
  encoders pad frames, looser than nothing because rounding is real.
- **Chapter sidecars, in both directions.** The checks above all read files *ffmpeg*
  wrote, which can only catch a reader defect; no byte this estate writes had ever been
  seen by an implementation that did not write it. That is the same blind spot that hid
  `mp4-core`'s `chpl` layout defect for several releases, when every `chpl` test had run
  against a box the crate wrote itself. So the sidecar round trip reverses the roles:
  the estate renders a `;FFMETADATA1` file and ffmpeg muxes it, and separately ffmpeg
  dumps a real corpus file with `-f ffmetadata` for the estate to read. Ten awkward
  titles go through it: `=`, `;`, `#`, a mid-title backslash, a newline, surrounding
  whitespace, one that looks like a section header, multi-byte UTF-8, quotes and percent
  signs, and an empty title.

Tolerances are deliberately different for the two, and the reason is worth stating:
a chapter boundary is **exact metadata written by the producer**, so shifting one is
misreporting a fact, whereas a duration is a measurement.

## A fixture that loses its chapters fails the run

Every corpus file declares how many chapters its recipe is supposed to produce, and
`Corpus::generate` checks that against what ffprobe actually reports. Without that check
the most dangerous failure available here is silent: a fixture that failed to encode its
chapters leaves *both* sides finding nothing, so every comparison passes while comparing
nothing.

That is not hypothetical. A hand-built `CHAP` frame that lost its sub-frame header
reported a clean pass for a whole iteration, which is why the expectation is asserted
where it can still stop the run, as `Error::FixtureFailed`.

## Not checked

`UNCHECKED_FIELDS` is printed on every run. Agreement is only ever agreement on what is
compared, and these are the things a reader could otherwise assume were verified: ID3
chapter end times, `CTOC` hierarchy and depth, `CHAP` byte offsets, and the
specification-shaped v2.3 frame header that no available tool reads.

## Honesty rules

- **Which mechanism supplied the chapters is recorded** (`mechanism: quicktime chapter
  track` on a real ffmpeg M4B). A harness reporting "6/6 agree" while quietly reading
  only one of the two chapter mechanisms would claim less coverage than it has, so a
  regression to `chpl`-only shows up as a change in that field rather than as a silent
  loss of checking.
- **The two chapter mechanisms are compared against each other**, not just against
  ffprobe. A file carrying both a Nero `chpl` box and a QuickTime chapter track can have
  them disagree, and a plain chapter comparison passes in that case because neither list
  is wrong on its own.
- **A field this harness does not compare is reported as *unchecked*, not as a pass.**
  A harness that only ever confirms the code is a test suite; this one is an
  adversary, and silence would be the one outcome that made it useless.
- **Absence is distinguished from zero.** ffprobe reporting no duration and this crate
  reporting no duration is agreement; ffprobe reporting one and the crate reporting
  none is a disagreement.
- **It does not skip what it cannot check.** A file whose chapters exist only in a form
  this harness does not yet read is reported as unverified rather than quietly passed.

As of the QuickTime chapter track landing, there are **no unchecked fields**: every
comparison above is either checked or reported.

## Requires

`ffmpeg` and `ffprobe` on `PATH`. Without an independent writer to generate a corpus
and an independent reader to check against, the harness would be verifying the code
against itself. It exits 3 with that message rather than proceeding.

## Extending it

Adding a comparison means adding a `Finding` variant and a `compare_*` function, then
wiring it into `check`. The existing two are the template: a variant that names both
the reference and the reader's value, a `Display` that prints both, and a test that
pins the disagreement case.

Symphonia is wired in as a dev-dependency so a third reader can be added as a second
opinion; a shared bug between two Rust crates is more likely than a shared bug between
a Rust crate and C, so `ffprobe` remains the reference.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.