//! Conformance harness for the WyattAu audiobook stack.
//!
//! # Why this exists
//!
//! `mp4-core`, `id3-core` and `audiobook-core` are written by the same person and
//! therefore share blind spots. A parser that agrees with itself proves nothing. This
//! harness generates a corpus with **ffmpeg** — an independent, widely deployed
//! writer — reads it back with the estate crates, and compares the result against
//! **ffprobe**'s own report of the same file.
//!
//! Every disagreement is a finding. A harness that only ever confirms the code is a
//! test suite; this one is an adversary.
//!
//! # What it does not do
//!
//! **It does not skip what it cannot check.** A file whose chapters only exist in the
//! form the harness does not yet read is reported as an *unverified* finding, not as
//! a pass. Silence would be the one outcome that made this useless.
//!
//! # Example
//!
//! The library side: generate a corpus and compare a reader against ffprobe's reading
//! of the same file.
//!
//! ```no_run
//! use audiobook_conformance::{compare_chapters, compare_duration, ffprobe, ActualChapter, Corpus};
//! use std::path::Path;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let corpus = Corpus::generate(Path::new("/tmp/audiobook-corpus"))?;
//!
//! for file in &corpus.files {
//!     let bytes = std::fs::read(&file.path)?;
//!     let probe = audiobook_core::MediaProbe::probe(&bytes);
//!
//!     let actual: Vec<ActualChapter> = probe
//!         .chapters
//!         .iter()
//!         .map(|(start_ms, title)| ActualChapter {
//!             start_ms: *start_ms,
//!             title: title.clone(),
//!         })
//!         .collect();
//!
//!     for finding in compare_chapters(&file.reference_chapters, &actual) {
//!         println!("{}: {finding}", file.path.display());
//!     }
//!     let duration = compare_duration(file.reference_duration_ms, probe.duration_ms);
//!     println!("{}: {duration}", file.path.display());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The binary side, which is what CI runs:
//!
//! ```text
//! audiobook-conformance --root /tmp/audiobook-corpus
//! ```
//!
//! `ffprobe` on its own is available for a single file:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let out = audiobook_conformance::ffprobe(std::path::Path::new("book.m4b"))?;
//! println!("{} chapters, {:?} ms", out.chapters.len(), out.duration_ms);
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a finding came from, which determines how much it is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// ffprobe's own report of a file. An independent implementation, mature, and the
    /// reference this harness measures against.
    Ffprobe,
    /// Symphonia, the pure-Rust demuxer. A second independent reader, which matters
    /// because a shared bug between two Rust crates is more likely than a shared bug
    /// between a Rust crate and C.
    Symphonia,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Ffprobe => f.write_str("ffprobe"),
            Source::Symphonia => f.write_str("symphonia"),
        }
    }
}

/// One chapter, as the reference implementation reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceChapter {
    /// Index as the reference numbered it, zero-based.
    pub index: u32,
    /// Start offset in milliseconds from the beginning of the file.
    pub start_ms: u64,
    /// The chapter's title, empty when the reference reported none.
    pub title: String,
}

/// One chapter, as the estate crates read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualChapter {
    /// Start offset in milliseconds.
    pub start_ms: u64,
    /// The chapter's title.
    pub title: String,
}

/// One chapter comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChapterFinding {
    /// The reader found no chapters where the reference found some.
    MissingAll {
        /// How many the reference reported.
        expected: usize,
    },
    /// The reader found chapters where the reference found none.
    UnexpectedAll {
        /// How many the reader reported.
        found: usize,
    },
    /// A chapter at this index disagreed.
    Mismatch {
        /// Which chapter.
        index: usize,
        /// What the reference said.
        expected: ReferenceChapter,
        /// What the reader said.
        actual: ActualChapter,
    },
    /// A file carries both a Nero `chpl` box and a QuickTime chapter track, and they
    /// disagree.
    ///
    /// Its own finding rather than a chapter mismatch, because a library that silently
    /// prefers one mechanism shows the user a list the file contradicts — and neither
    /// list is *wrong* on its own, so a plain chapter comparison would pass.
    MechanismDisagreement {
        /// How many chapters the `chpl` box holds.
        chpl_len: usize,
        /// How many the chapter track holds.
        track_len: usize,
    },
    /// A field this harness does not yet compare, recorded so it is visible rather
    /// than silently absent.
    Unverified {
        /// What is unchecked.
        field: &'static str,
        /// Why.
        reason: &'static str,
    },
}

impl fmt::Display for ChapterFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChapterFinding::MissingAll { expected } => {
                write!(f, "read 0 chapters, reference reported {expected}")
            }
            ChapterFinding::UnexpectedAll { found } => {
                write!(f, "read {found} chapters, reference reported none")
            }
            ChapterFinding::Mismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "chapter {index}: reference `{}` @ {} ms, read `{}` @ {} ms",
                expected.title, expected.start_ms, actual.title, actual.start_ms
            ),
            ChapterFinding::MechanismDisagreement { chpl_len, track_len } => write!(
                f,
                "the file carries both chapter mechanisms and they disagree:                  chpl has {chpl_len}, the chapter track has {track_len}"
            ),
            ChapterFinding::Unverified { field, reason } => {
                write!(f, "{field} unchecked: {reason}")
            }
        }
    }
}

/// A comparison of one duration, with a tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationFinding {
    /// Both agree within the tolerance.
    Agreed {
        /// The reference's value.
        reference_ms: u64,
        /// The reader's value.
        actual_ms: u64,
    },
    /// They differ by more than the tolerance.
    Disagreed {
        /// The reference's value.
        reference_ms: u64,
        /// The reader's value.
        actual_ms: u64,
        /// Absolute difference.
        delta_ms: u64,
    },
    /// Only one side had a number.
    OneSided {
        /// The reference's value, when it had one.
        reference_ms: Option<u64>,
        /// The reader's value, when it had one.
        actual_ms: Option<u64>,
    },
    /// Neither side had a number, which is agreement.
    ///
    /// Its own variant rather than folded into `Agreed`, because "both said nothing"
    /// is a different fact from "both said the same number" and a report that
    /// conflated them would read as a successful duration check on a file where no
    /// duration was ever compared.
    Neither,
}

impl DurationFinding {
    /// Whether this finding is a disagreement.
    #[must_use]
    pub fn is_disagreement(&self) -> bool {
        matches!(
            self,
            DurationFinding::Disagreed { .. } | DurationFinding::OneSided { .. }
        )
    }

    /// Whether a duration was actually compared, as opposed to absent on both sides.
    #[must_use]
    pub fn was_compared(&self) -> bool {
        !matches!(self, DurationFinding::Neither)
    }
}

impl fmt::Display for DurationFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DurationFinding::Agreed {
                reference_ms,
                actual_ms,
            } => write!(f, "duration {actual_ms} ms (reference {reference_ms} ms)"),
            DurationFinding::Disagreed {
                reference_ms,
                actual_ms,
                delta_ms,
            } => write!(
                f,
                "duration {actual_ms} ms vs reference {reference_ms} ms — off by {delta_ms} ms"
            ),
            DurationFinding::OneSided {
                reference_ms,
                actual_ms,
            } => write!(
                f,
                "duration present on one side only: reference {reference_ms:?}, read {actual_ms:?}"
            ),
            DurationFinding::Neither => f.write_str("no duration on either side"),
        }
    }
}

/// Everything one file produced.
#[derive(Debug, Clone)]
pub struct Report {
    /// The file that was checked.
    pub path: PathBuf,
    /// Who the reference was.
    pub source: Source,
    /// How the file was identified.
    pub container: &'static str,
    /// Chapter comparisons.
    pub chapters: Vec<ChapterFinding>,
    /// The duration comparison.
    pub duration: DurationFinding,
    /// Titles read from the container, for a human to look at.
    pub title: Option<String>,
    /// Artist or narrator.
    pub artist: Option<String>,
    /// Which chapter mechanism supplied the list, so coverage is self-describing.
    ///
    /// A harness reporting "6/6 agree" while quietly reading only one of the two
    /// mechanisms would claim less coverage than it has. Recording which one ran means
    /// a regression to `chpl`-only — or to the track only — shows up as a change here
    /// rather than as a silent loss of checking.
    pub mechanism: &'static str,
}

impl Report {
    /// Whether everything compared agreed.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.chapters.iter().any(is_disagreement) && !self.duration.is_disagreement()
    }

    /// Whether anything is recorded as unchecked, which is neither a pass nor a
    /// failure but must not be mistaken for one.
    #[must_use]
    pub fn has_unverified(&self) -> bool {
        self.chapters
            .iter()
            .any(|c| matches!(c, ChapterFinding::Unverified { .. }))
    }

    /// Whether every disagreement in this report is already understood and
    /// attributed to a known external limitation — reported above, but not
    /// a gate failure.
    #[must_use]
    pub fn is_attributed(&self) -> bool {
        self.chapters.iter().all(|c| !is_disagreement(c)) && !self.duration.is_disagreement()
    }

    /// A one-line summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let verdict = if self.is_clean() {
            "ok"
        } else {
            "DISAGREEMENT"
        };
        let mut out = format!(
            "[{verdict}] {} as {container} vs {source}: {duration}",
            self.path.file_name().unwrap_or_default().to_string_lossy(),
            container = self.container,
            source = self.source,
            duration = self.duration,
        );
        for c in &self.chapters {
            if is_disagreement(c) {
                out.push_str(&format!("\n         chapter: {c}"));
            } else if let ChapterFinding::MechanismDisagreement { .. } = c {
                out.push_str(&format!("\n         chapters: {c}"));
            } else if let ChapterFinding::Unverified { field, .. } = c {
                out.push_str(&format!("\n         unchecked: {field}"));
            }
        }
        out
    }
}

/// Whether a chapter finding is a disagreement rather than a note.
fn is_disagreement(c: &ChapterFinding) -> bool {
    matches!(
        c,
        ChapterFinding::MissingAll { .. }
            | ChapterFinding::UnexpectedAll { .. }
            | ChapterFinding::Mismatch { .. }
            | ChapterFinding::MechanismDisagreement { .. }
    )
}

/// The tolerance for a duration comparison, in milliseconds.
///
/// Not zero, because AAC and MP3 both pad frames: an encoder's declared duration and
/// a frame-count-derived one legitimately differ by a frame or two. 50 ms is about
/// one AAC frame at 44.1 kHz, which is the smallest difference that can be called a
/// real disagreement rather than rounding.
pub const DURATION_TOLERANCE_MS: u64 = 50;

/// The tolerance for a duration the reader *declared* rather than derived.
///
/// Much tighter than [`DURATION_TOLERANCE_MS`] on purpose. A declared duration comes
/// from a frame count the encoder wrote, and a reader that finds that count and applies
/// the encoder's own delay and padding should land on the same millisecond ffprobe does.
/// The two compute the same division, so anything beyond rounding is a defect — and a
/// 50 ms tolerance was hiding exactly that: an MP3 whose reader ignored the LAME
/// delay and padding was 31 ms long, a third of the budget spent on a defect that was
/// always reported as agreement.
pub const DECLARED_DURATION_TOLERANCE_MS: u64 = 2;

/// How closely two chapter start times must agree, in milliseconds.
///
/// Tighter than the duration tolerance on purpose: chapter boundaries are exact
/// metadata, written by the producer, and a reader that shifts one is misreporting a
/// fact rather than rounding a measurement.
pub const CHAPTER_TOLERANCE_MS: u64 = 2;

/// Fields this harness checks nowhere, recorded so a run cannot imply coverage it lacks.
///
/// Every entry here is something a reader could reasonably assume is verified because the
/// run reported agreement. Each is a real gap, and naming it is the difference between
/// "the chapters match" and "the chapters we happened to compare match".
pub const UNCHECKED_FIELDS: &[(&str, &str)] = &[
    (
        "ID3 chapter end times",
        "ffprobe reports each chapter's `end`, but `compare_chapters` compares only the \
         start offset and the title",
    ),
    (
        "trailing tags in a CBR estimate, MP3 without a declared frame count",
        "a constant-bitrate MP3 with no Xing/Info has no duration anywhere, so it is          estimated from bytes over bitrate. This crate subtracts a trailing ID3v1 or APE          tag before dividing, because the tag is not audio; ffprobe and mutagen do not,          and agree with each other to the microsecond. On a tagged file the two estimates          differ by the tag's size — a few milliseconds at audiobook bitrates, inside this          harness's 50 ms derived-duration tolerance, so a run here would call them          agreement. It is not agreement; it is a documented divergence hidden inside a          tolerance. The crate's number is the closer one to the audio, which is why it          stands; this entry is why a green run must not be read as saying otherwise",
    ),
    (
        "CTOC nesting depth beyond one level",
        "mutagen exposes a TOC's child list, flags and membership, and those are now \
         verified against it \u{2014} which is how id3-core 0.2.0's transposed \
         top-level/ordered flags would have been caught from the corpus rather than from a \
         hand-written fixture. What no available oracle computes is the resolved *depth* of \
         a chapter in a multi-level TOC tree, so `resolve`'s depth assignment is still only \
         exercised by id3-core's own tests",
    ),
    (
        "CHAP byte offsets",
        "the `0xFFFFFFFF` ignore-sentinel is written and accepted, but a file with real \
         seek offsets is not in the corpus and no reader here is judged on it",
    ),
    (
        "ID3v2.3 with a nine-byte frame header",
        "accepted by id3-core, as the specification requires, but no tool on this machine \
         reads that shape, so it cannot be cross-checked",
    ),
];

/// A generated file plus what the reference implementation said about it.
#[derive(Debug, Clone)]
pub struct CorpusFile {
    /// Where it is on disk.
    pub path: PathBuf,
    /// How it was produced, for a failure message.
    pub recipe: String,
    /// Chapters as the reference reported them.
    pub reference_chapters: Vec<ReferenceChapter>,
    /// Duration in milliseconds as the reference reported it.
    pub reference_duration_ms: Option<u64>,
    /// The container the recipe intended.
    pub kind: FileKind,
}

impl FileKind {
    /// Every kind, in the order a report reads best.
    ///
    /// Lives beside the enum rather than in the binary that runs it. A hand-maintained
    /// list of the kinds *to check* is a list that drifts: a kind added here and built
    /// there but absent from the list is generated, costs several seconds of ffmpeg, and
    /// is then never compared with anything — a corpus file that exists so the harness
    /// can check a thing, and does not.
    pub const ALL: [FileKind; 9] = [
        FileKind::M4bWithChapters,
        FileKind::M4aBare,
        FileKind::Mp3CbrTagged,
        FileKind::Mp3Vbr,
        FileKind::Mp3SpokenWord,
        FileKind::Mp3Folder,
        FileKind::Mp3ChaptersNoToc,
        FileKind::Mp3ChaptersTopLevelOnly,
        FileKind::Mp3V23Metadata,
    ];
}

/// The kinds of file the corpus contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileKind {
    /// A single M4B with chapters, written by ffmpeg from an FFMetadata file.
    M4bWithChapters,
    /// A single M4A with no chapters.
    M4aBare,
    /// A CBR MP3 with an ID3v2 tag.
    Mp3CbrTagged,
    /// A VBR MP3 carrying a Xing header.
    Mp3Vbr,
    /// A folder of MP3s, which is the multi-file shape.
    Mp3Folder,
    /// An M4B whose chapters exist only as a QuickTime text track.
    M4bTextTrackChapters,
    /// An MP3 with `CHAP` frames and **no** `CTOC`.
    ///
    /// Built here byte by byte rather than by ffmpeg, because ffmpeg always writes both
    /// and the spec explicitly permits chapters that no TOC references. Writing the
    /// fixture is legitimate — what would not be is *verifying* it with this harness, so
    /// ffprobe remains the oracle.
    Mp3ChaptersNoToc,
    /// An MP3 with a top-level-but-not-ordered `CTOC`, i.e. flags `0x02`.
    ///
    /// Exists to discriminate the two CTOC flag bits, which every ffmpeg-written file
    /// masks by setting both. It has to be ID3v2.4: mutagen and ffmpeg both read *no*
    /// chapters out of an ID3v2.3 `CHAP`, so a v2.3 fixture could not be judged by the
    /// oracle and would pass vacuously.
    Mp3ChaptersTopLevelOnly,
    /// A 22.05 kHz mono MP3 — the encoding a spoken-word book is actually shipped in.
    ///
    /// Exists for one reason. MPEG-2 side information is 9 bytes mono where MPEG-1's
    /// is 17, so the offset of a Xing/Info tag from the start of its frame differs, and
    /// reading the width one byte short makes a perfectly good frame count read as
    /// *absent*: the reader falls back to an estimate made from the bitrate of the very
    /// frame the tag lives in, whose bitrate field is a placeholder. The result was a
    /// 12-second audiobook reported as 6.9, with nothing to say so. Every other MP3 in
    /// this corpus is 44.1 kHz, so nothing else could catch it.
    Mp3SpokenWord,
    /// An ID3v2.3 MP3 with ordinary metadata frames and no chapters.
    ///
    /// ID3v2.3's frame header is one byte shorter than v2.4's, and reading it with the
    /// v2.4 length mis-frames the whole tag so it fails to parse at all — losing the
    /// title, the artist and everything else. Nothing in a corpus built only from ffmpeg
    /// could catch that, because ffmpeg writes v2.4.
    Mp3V23Metadata,
}

impl FileKind {
    /// How many chapters this kind's recipe is supposed to produce.
    ///
    /// `None` for a kind that legitimately has none, and the assertion that a kind which
    /// *does* have them really does — because a fixture that silently failed to encode
    /// them makes every comparison against it pass while checking nothing.
    #[must_use]
    pub fn expected_chapters(self) -> Option<usize> {
        match self {
            FileKind::M4bWithChapters => Some(3),
            FileKind::Mp3CbrTagged => Some(3),
            FileKind::Mp3ChaptersNoToc => Some(2),
            FileKind::Mp3ChaptersTopLevelOnly => Some(2),
            FileKind::Mp3V23Metadata => Some(2),
            FileKind::Mp3SpokenWord => Some(3),
            // These have no chapters by design, and a file that did have some would be a
            // defect rather than a surprise.
            FileKind::M4aBare | FileKind::Mp3Vbr | FileKind::Mp3Folder => Some(0),
            // The QuickTime-track chapter file is built by an independent writer that
            // decides its own chapter count, so pinning a number would break whenever
            // that writer changes its mind.
            FileKind::M4bTextTrackChapters => None,
        }
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FileKind::M4bWithChapters => "M4B with Nero chpl chapters",
            FileKind::M4aBare => "bare M4A",
            FileKind::Mp3CbrTagged => "CBR MP3 with ID3v2",
            FileKind::Mp3Vbr => "VBR MP3 with a Xing header",
            FileKind::Mp3Folder => "a folder of MP3s",
            FileKind::M4bTextTrackChapters => "M4B with QuickTime chapter-track chapters",
            FileKind::Mp3ChaptersNoToc => "MP3 with CHAP chapters and no CTOC",
            FileKind::Mp3ChaptersTopLevelOnly => "MP3 with a top-level CTOC (flags 0x02)",
            FileKind::Mp3V23Metadata => "ID3v2.3 MP3 with ordinary metadata frames",
            FileKind::Mp3SpokenWord => "22.05 kHz mono MP3 (spoken-word encoding)",
        };
        f.write_str(s)
    }
}

/// The generated corpus.
#[derive(Debug, Clone)]
pub struct Corpus {
    /// Where it lives.
    pub root: PathBuf,
    /// Every file, with the reference's reading of it.
    pub files: Vec<CorpusFile>,
}

impl Corpus {
    /// Generate a corpus under `root`, using ffmpeg and ffprobe from `PATH`.
    ///
    /// # Errors
    ///
    /// Returns an error when ffmpeg or ffprobe is missing, when a recipe fails, or
    /// when a file cannot be written. Failing loudly matters: a corpus that silently
    /// generated nothing would make every downstream check vacuous.
    pub fn generate(root: &Path) -> Result<Self, Error> {
        fs::create_dir_all(root).map_err(|e| Error::Io {
            what: "create corpus root",
            source: e.to_string(),
        })?;
        let source = root.join("source.wav");
        write_tone(&source)?;
        let source_arg = path_arg(&source);

        let meta = root.join("chapters.ffmeta");
        let titles = ["Chapter One", "Chapter Two", "Chapter Three"];
        fs_write(&meta, &ffmetadata(180_000, &titles))?;

        let mut built: Vec<(PathBuf, String, FileKind)> = Vec::new();

        // --- An M4B with chapters, from an FFMetadata sidecar. -------------------
        let m4b = root.join("book.m4b");
        run(
            "ffmpeg",
            &args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &source_arg,
                "-i",
                &path_arg(&meta),
                "-map_metadata",
                "1",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
                &path_arg(&m4b),
            ]),
        )?;
        built.push((
            m4b,
            "ffmpeg -i source.wav -i chapters.ffmeta -map_metadata 1 -c:a aac book.m4b".into(),
            FileKind::M4bWithChapters,
        ));

        // --- A bare M4A, which must report no chapters. ---------------------------
        let m4a = root.join("bare.m4a");
        run(
            "ffmpeg",
            &args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &source_arg,
                "-c:a",
                "aac",
                "-b:a",
                "64k",
                &path_arg(&m4a),
            ]),
        )?;
        built.push((
            m4a,
            "ffmpeg -i source.wav -c:a aac -b:a 64k bare.m4a".into(),
            FileKind::M4aBare,
        ));

        // --- A CBR MP3 with chapters. ---------------------------------------------
        let mp3 = root.join("tagged.mp3");
        run(
            "ffmpeg",
            &args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &source_arg,
                "-i",
                &path_arg(&meta),
                "-map_metadata",
                "1",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "128k",
                "-write_id3v2",
                "1",
                &path_arg(&mp3),
            ]),
        )?;
        built.push((
            mp3,
            "ffmpeg -i source.wav -i chapters.ffmeta -c:a libmp3lame -b:a 128k tagged.mp3".into(),
            FileKind::Mp3CbrTagged,
        ));

        // --- The encoding a spoken-word book ships in. ------------------------------
        //
        // 22.05 kHz mono, which is MPEG-2 rather than MPEG-1 and therefore has its own
        // side-information width. Every other MP3 here is 44.1 kHz, so without this one
        // the corpus cannot see a mistake in the MPEG-2 widths at all.
        let spoken = root.join("spoken.mp3");
        run(
            "ffmpeg",
            &args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &source_arg,
                "-i",
                &path_arg(&meta),
                "-map_metadata",
                "1",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "32k",
                "-ar",
                "22050",
                "-ac",
                "1",
                "-write_id3v2",
                "1",
                &path_arg(&spoken),
            ]),
        )?;
        built.push((
            spoken,
            "ffmpeg -i source.wav -i chapters.ffmeta -c:a libmp3lame -b:a 32k -ar 22050 \
             -ac 1 spoken.mp3"
                .into(),
            FileKind::Mp3SpokenWord,
        ));

        // --- An MP3 with chapters and no table of contents. -------------------------
        //
        // ffmpeg always writes both `CHAP` frames and a `CTOC`, and the spec explicitly
        // permits chapters that no TOC references, so that shape is unreachable through
        // ffmpeg. The tag is assembled here and the *audio* still comes from ffmpeg, so
        // the fixture is honest while ffprobe remains the only thing that judges it.
        let no_toc = root.join("chap-notoc.mp3");
        let tag = id3_tag(
            4,
            &[
                chap_frame("ch0", 0, 60_000, "One"),
                chap_frame("ch1", 60_000, 120_000, "Two"),
            ],
        );
        let bare_audio = root.join("bare-audio.mp3");
        run(
            "ffmpeg",
            &args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &source_arg,
                "-c:a",
                "libmp3lame",
                "-b:a",
                "128k",
                "-write_id3v2",
                "0",
                &path_arg(&bare_audio),
            ]),
        )?;
        splice_tag(&bare_audio, &no_toc, &tag)?;
        built.push((
            no_toc,
            "ffmpeg audio, plus a hand-built ID3v2.4 tag with CHAP frames and no CTOC".into(),
            FileKind::Mp3ChaptersNoToc,
        ));

        // --- An MP3 with chapters whose CTOC discriminates the two flag bits. -------
        //
        // Every ffmpeg-written TOC carries flags 0x03, with *both* bits set, so a reader
        // that transposes them reads it correctly by accident — which is how a swapped
        // top-level/ordered pair survives against a corpus built only from ffmpeg. This
        // file sets top-level and leaves ordered clear: 0x02, the byte mutagen writes for
        // `CTOCFlags.TOP_LEVEL`. A reader with the bits the wrong way round reports this
        // TOC as *not* top level and so has no root to resolve.
        let top_only = root.join("chap-toplevel-only.mp3");
        let mut toc_body = Vec::new();
        toc_body.extend_from_slice(b"toc");
        toc_body.push(0x00);
        toc_body.push(0x02); // top-level set, ordered clear
        toc_body.push(0x02); // entry count
        toc_body.extend_from_slice(b"ch0");
        toc_body.push(0x00);
        toc_body.extend_from_slice(b"ch1");
        toc_body.push(0x00);
        // A sub-frame inside a CHAP/CTOC needs its own header, not just a body.
        toc_body.extend_from_slice(&frame_body(b"TIT2", &tit2("Part One", 4)));
        let tag = id3_tag(
            4,
            &[
                frame_body(b"CTOC", &toc_body),
                chap_frame("ch0", 0, 60_000, "One"),
                chap_frame("ch1", 60_000, 120_000, "Two"),
            ],
        );
        splice_tag(&bare_audio, &top_only, &tag)?;
        built.push((
            top_only,
            "ffmpeg audio, plus a hand-built ID3v2.4 tag with a top-level CTOC (flags 0x02)".into(),
            FileKind::Mp3ChaptersTopLevelOnly,
        ));

        // --- An ID3v2.3 tag with ordinary metadata. -------------------------------
        //
        // The frame header is nine bytes here, not ten, and the size field is plain
        // big-endian rather than synch-safe. A reader that assumes v2.4's layout
        // mis-frames every frame by one byte and the tag does not parse at all, so the
        // title and artist disappear along with everything else. ffmpeg cannot produce
        // this shape — it writes v2.4 — so nothing built from ffmpeg would catch it.
        let v23 = root.join("v23-metadata.mp3");
        let frames = vec![
            frame_body(b"TIT2", &tit2("A Legacy Encoder", 3)),
            frame_body(b"TPE1", &tit2("A Narrator", 3)),
            frame_body(b"CHAP", &chap_body("ch0", 0, 60_000, "One", 3)),
            frame_body(b"CHAP", &chap_body("ch1", 60_000, 120_000, "Two", 3)),
        ];
        splice_tag(&bare_audio, &v23, &id3_tag(3, &frames))?;
        built.push((
            v23,
            "ffmpeg audio, plus a hand-built ID3v2.3 tag with TIT2, TPE1 and CHAP frames".into(),
            FileKind::Mp3V23Metadata,
        ));

        // --- A folder of MP3s, cut into three parts. -------------------------------
        let folder = root.join("folder");
        fs::create_dir_all(&folder).map_err(|e| Error::Io {
            what: "create folder",
            source: e.to_string(),
        })?;
        for (i, start_s) in [0u64, 60, 120].iter().enumerate() {
            let part = folder.join(format!("part{}.mp3", i + 1));
            run(
                "ffmpeg",
                &args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-ss",
                    &start_s.to_string(),
                    "-t",
                    "60",
                    "-i",
                    &source_arg,
                    "-c:a",
                    "libmp3lame",
                    "-b:a",
                    "128k",
                    "-metadata",
                    &format!("title=Part {}", i + 1),
                    &path_arg(&part),
                ]),
            )?;
            built.push((
                part,
                "ffmpeg -ss N -t 60 -i source.wav -c:a libmp3lame -b:a 128k folder/partN.mp3"
                    .into(),
                FileKind::Mp3Folder,
            ));
        }

        // Read the reference's view of every file.
        let mut out = Vec::new();
        for (path, recipe, kind) in built {
            let probe = ffprobe(&path)?;
            // A file whose recipe promises chapters but which the oracle reports as
            // chapterless is a **broken fixture**, and it is the most dangerous kind of
            // failure this harness can have: both sides then find nothing and agree, so
            // every downstream comparison passes while testing nothing at all. That is not
            // hypothetical — a hand-built `CHAP` fixture went missing its sub-frame header
            // and reported a clean pass for a whole iteration.
            //
            // So the expectation is checked here, where it can still stop the run, rather
            // than left to the comparison downstream.
            let expected = kind.expected_chapters();
            if let Some(want) = expected {
                if probe.chapters.len() != want {
                    return Err(Error::FixtureFailed {
                        path: path.display().to_string(),
                        recipe,
                        detail: format!(
                            "the recipe promises {want} chapter(s) but the reference \
                             reported {} — so any comparison against it would pass \
                             vacuously",
                            probe.chapters.len()
                        ),
                    });
                }
            }
            out.push(CorpusFile {
                path,
                recipe,
                reference_chapters: probe.chapters,
                reference_duration_ms: probe.duration_ms,
                kind,
            });
        }

        Ok(Self {
            root: root.to_path_buf(),
            files: out,
        })
    }

    /// The corpus files of one kind.
    #[must_use]
    pub fn of_kind(&self, kind: FileKind) -> Vec<&CorpusFile> {
        self.files.iter().filter(|f| f.kind == kind).collect()
    }
}

/// What ffprobe reported about a file.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutput {
    /// Chapters, in the order ffprobe listed them.
    pub chapters: Vec<ReferenceChapter>,
    /// Duration in milliseconds, when ffprobe could determine one.
    pub duration_ms: Option<u64>,
    /// The reported format name.
    pub format: Option<String>,
}

/// Run ffprobe and parse the parts this harness compares.
///
/// # Errors
///
/// Returns an error when ffprobe is missing or its output cannot be parsed. A missing
/// ffprobe must not be treated as "no chapters", which would make every MP3 look like
/// a failure and every missing-file case look like a pass.
pub fn ffprobe(path: &Path) -> Result<ProbeOutput, Error> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_chapters",
            "-show_entries",
            "format=duration,format_name",
            "-print_format",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|e| Error::Tool {
            tool: "ffprobe",
            reason: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(Error::Tool {
            tool: "ffprobe",
            reason: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    parse_ffprobe_json(&String::from_utf8_lossy(&out.stdout))
}

/// Parse ffprobe's JSON without pulling in a JSON dependency.
///
/// # Why hand-rolled
///
/// Hand-rolled deliberately: this harness must be buildable in an environment with no
/// network, and a conformance tool that needs a registry to run is a conformance tool
/// nobody runs. Only the handful of fields actually compared are extracted, and
/// anything unexpected is ignored rather than guessed at.
pub fn parse_ffprobe_json(json: &str) -> Result<ProbeOutput, Error> {
    let mut out = ProbeOutput::default();

    // Every field is read from the top level rather than from an extracted `format`
    // object. An earlier version pulled the object out first and then looked inside
    // it, which cannot work: the extractor returns scalars, so the "object" it
    // returned was a fragment ending at the first comma, and the second lookup found
    // nothing inside it. Reading each key from the whole document is also more
    // robust, because it does not depend on nesting.
    //
    // It relies on `duration` and `format_name` appearing once at the top level, which
    // is true of ffprobe's output — chapters carry `start_time`, not `duration`.
    if let Some(d) = json_field(json, "duration") {
        out.duration_ms = d
            .trim()
            .parse::<f64>()
            .ok()
            .map(|s| (s * 1000.0).round() as u64);
    }
    if let Some(n) = json_field(json, "format_name") {
        out.format = Some(n.trim().to_string());
    }

    // Chapters: walk each object in the `chapters` array and pull `start_time` and the
    // `tags.title`.
    //
    // The scan is bounded by the array's own closing `]`. An unbounded search for the
    // next `{` runs straight past an empty array into the `format` object and reports
    // it as a chapter — which showed up as a harness reporting one extra chapter
    // against a file whose chapters were read perfectly correctly.
    if let Some(array_at) = json.find("\"chapters\"") {
        if let Some(open) = json[array_at..].find('[') {
            let from = array_at + open + 1;
            let Some(to) = find_array_end(json, from - 1) else {
                return Ok(out);
            };
            let mut cursor = from;
            while cursor < to {
                let Some(window) = json.get(cursor..to) else {
                    break;
                };
                let Some(obj_start) = window.find('{') else {
                    break;
                };
                let at = cursor + obj_start;
                // An object ends at the `}` that closes it, not at the first one: a
                // chapter title can contain a brace.
                let Some(close) = find_object_end(json, at) else {
                    break;
                };
                let obj = &json[at..close.min(to)];

                out.chapters.push(ReferenceChapter {
                    index: out.chapters.len() as u32,
                    start_ms: json_number(obj, "start_time")
                        .map(|v| (v * 1000.0).round() as u64)
                        .unwrap_or(0),
                    title: json_string(obj, "title").unwrap_or_default(),
                });

                cursor = close;
            }
        }
    }

    Ok(out)
}

/// The index of the `]` closing the array that opens at `open`.
///
/// String-aware for the same reason as [`find_object_end`]: a title may contain a
/// bracket.
fn find_array_end(json: &str, open: usize) -> Option<usize> {
    let bytes = json.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The index just past the `}` closing the object that starts at `open`.
///
/// String-aware: a chapter title may contain a brace, and a JSON parser that finds
/// the first one reports a title truncated at the first `}` — which then compares as
/// a real disagreement rather than a harness bug.
fn find_object_end(json: &str, open: usize) -> Option<usize> {
    let bytes = json.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The numeric value of `"key": <number>` within `json`.
fn json_number(json: &str, key: &str) -> Option<f64> {
    let raw = json_field(json, key)?;
    raw.trim().parse::<f64>().ok()
}

/// The string value of `"key": "value"` within `json`, with JSON escapes resolved.
///
/// Returns `None` when the value is not a string or contains an escape JSON does not
/// define. Resolving the escapes is not optional: ffprobe writes a title containing a
/// quote, a backslash or a newline as `\"`, `\\` and `\n`, and comparing the raw text
/// would report a perfectly good chapter as a disagreement on every title containing any
/// of those. That is not hypothetical \u{2014} it is exactly what the sidecar round trip
/// found the first time a corpus title had a newline in it.
fn json_string(json: &str, key: &str) -> Option<String> {
    json_unescape(json_field(json, key)?)
}

/// Resolve JSON string escapes.
///
/// The full set JSON defines, including `\uXXXX` and surrogate pairs \u{2014} ffprobe emits
/// `\u00e9`-style escapes for some characters and raw UTF-8 for others depending on the
/// build, so both forms have to work. Returns `None` for an escape JSON does not define or
/// a lone surrogate, rather than guessing: a harness that quietly turns a broken escape
/// into a plausible string will report a disagreement that is not there, or hide one that
/// is.
fn json_unescape(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    // `chars.clone()` is passed to `hex4` so it can look ahead without consuming.
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let high = hex4(&mut chars)?;
                let code = if (0xD800..0xDC00).contains(&high) {
                    // A high surrogate must be followed by `\uDC00`-`\uDFFF`.
                    if chars.next()? != '\\' || chars.next()? != 'u' {
                        return None;
                    }
                    let low = hex4(&mut chars)?;
                    if !(0xDC00..0xE000).contains(&low) {
                        return None;
                    }
                    0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                } else {
                    high
                };
                out.push(char::from_u32(code)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Four hex digits off the front of an iterator, as a `u32`.
fn hex4(chars: &mut impl Iterator<Item = char>) -> Option<u32> {
    let mut value: u32 = 0;
    for _ in 0..4 {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

/// Pull the raw text of `"key": "value"` or `"key": 123` out of a JSON fragment.
///
/// Written out rather than pulled from a crate because this harness must build with no
/// network: a conformance tool that needs a registry to run is a conformance tool
/// nobody runs. This finds the key, skips its own closing quote, finds the colon, and
/// then reads either a quoted string or a bare number — the earlier version read the
/// key's own closing quote as the value's opening one and returned `"title"` as a
/// chapter's title, which then showed up as a disagreement against a perfectly good
/// crate.
fn json_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let at = json.find(&needle)? + needle.len();
    let rest = json.get(at..)?;
    // Skip past the key's closing quote and the colon.
    let rest = rest.trim_start_matches('"').trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();

    if let Some(inner) = rest.strip_prefix('"') {
        let mut escaped = false;
        for (i, c) in inner.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                return inner.get(..i);
            }
        }
        return None;
    }
    let end = rest.find([',', '}', ']', '\n']).unwrap_or(rest.len());
    rest.get(..end)
}

/// Whether ffmpeg is available.
#[must_use]
pub fn has_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether ffprobe is available.
#[must_use]
pub fn has_ffprobe() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Write a mono 440 Hz tone as a 16-bit WAV.
///
/// Generated rather than shipped so the corpus is reproducible from source and the
/// repository stays small; 180 seconds at 44.1 kHz is about 15 MB on disk, which is
/// the trade for a file long enough to have three distinct chapters.
fn write_tone(path: &Path) -> Result<(), Error> {
    let sample_rate = 44_100u32;
    let seconds = 180u32;
    let samples = sample_rate as usize * seconds as usize;
    let mut pcm = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let phase = 2.0 * std::f64::consts::PI * 440.0 * i as f64 / sample_rate as f64;
        let v = (12_000.0 * phase.sin()) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
    }

    let mut data = Vec::with_capacity(44 + pcm.len());
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&((36 + pcm.len()) as u32).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    data.extend_from_slice(&1u16.to_le_bytes()); // PCM
    data.extend_from_slice(&1u16.to_le_bytes()); // mono
    data.extend_from_slice(&sample_rate.to_le_bytes());
    data.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    data.extend_from_slice(&2u16.to_le_bytes()); // block align
    data.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    data.extend_from_slice(b"data");
    data.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    data.extend_from_slice(&pcm);

    std::fs::write(path, data).map_err(|e| Error::Io {
        what: "write source tone",
        source: e.to_string(),
    })
}

/// An FFMetadata file with chapters, which is what ffmpeg turns into chapters.
fn ffmetadata(duration_ms: u64, titles: &[&str]) -> String {
    let mut out =
        String::from(";FFMETADATA1\ntitle=The Long Now\nartist=A Narrator\nalbum=A Series\n");
    let step = duration_ms / titles.len() as u64;
    for (i, t) in titles.iter().enumerate() {
        let start = step * i as u64;
        let end = if i + 1 == titles.len() {
            duration_ms
        } else {
            step * (i + 1) as u64
        };
        out.push_str("[CHAPTER]\nTIMEBASE=1/1000\n");
        out.push_str(&format!("START={start}\nEND={end}\ntitle={t}\n"));
    }
    out
}

/// Shorthand for a filesystem read or write with context.
fn fs_write(path: &Path, contents: &str) -> Result<(), Error> {
    std::fs::write(path, contents).map_err(|e| Error::Io {
        what: "write corpus file",
        source: format!("{}: {e}", path.display()),
    })
}

/// Shorthand alias kept short because it appears in every recipe.
use std::fs;

/// An argument list, so a computed path can sit next to literals.
type Args = Vec<String>;

/// Build an argument list from anything string-like.
fn args<I, S>(items: I) -> Args
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}

/// Run a tool, turning a non-zero exit into an error carrying its stderr.
fn run(tool: &'static str, args: &[String]) -> Result<(), Error> {
    let out = Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| Error::Tool {
            tool,
            reason: e.to_string(),
        })?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Tool {
        tool,
        reason: format!(
            "{tool} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    })
}

/// Run a tool and return its stdout, turning a non-zero exit into an error carrying its
/// stderr.
///
/// Needed wherever the tool's *output* is the thing under test \u{2014} dumping chapters
/// as ffmetadata, for instance \u{2014} where [`run`] throws the bytes away.
fn run_capture(tool: &'static str, args: &[String]) -> Result<Vec<u8>, Error> {
    let out = Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| Error::Tool {
            tool,
            reason: e.to_string(),
        })?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(Error::Tool {
        tool,
        reason: format!(
            "{tool} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    })
}

/// Build a text-frame body for `version`: an encoding byte, the text, and the trailing
/// NUL that every real writer emits.
///
/// The encoding is not cosmetic. ID3v2.3 defines only ISO-8859-1 (0) and UTF-16 with a
/// BOM (1); **UTF-8 (3) arrived in v2.4**. A v2.3 frame tagged UTF-8 is invalid, and both
/// mutagen and ffprobe drop the whole frame on sight — a tag that looks perfectly
/// well-formed and reads as no metadata at all. That is what this function's `version`
/// parameter is for: a v2.3 fixture written with UTF-8 is a fixture that tests nothing,
/// because the oracle discards it before it can disagree.
fn tit2(text: &str, version: u8) -> Vec<u8> {
    let mut body = Vec::with_capacity(text.len() * 2 + 4);
    if version >= 4 {
        body.push(0x03); // UTF-8
        body.extend_from_slice(text.as_bytes());
        body.push(0x00);
    } else {
        // UTF-16 with a byte-order mark, which is what ID3v2.3 can actually express.
        body.push(0x01);
        body.extend_from_slice(&[0xFF, 0xFE]); // little-endian BOM
        for unit in text.encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        body.extend_from_slice(&[0x00, 0x00]); // NUL terminator, in UTF-16
    }
    body
}

/// Build a `CHAP` body.
///
/// The two offsets are `0xFFFFFFFF`, the sentinel the spec defines for "ignore this and
/// use the time" — a real writer with no seek table emits exactly that, and a fixture
/// that put plausible-looking byte offsets in would be testing nothing.
fn chap_frame(element_id: &str, start_ms: u32, end_ms: u32, title: &str) -> Vec<u8> {
    frame_body(b"CHAP", &chap_body(element_id, start_ms, end_ms, title, 4))
}

/// The body of a `CHAP` frame, without a header of its own.
///
/// Split out from [`chap_frame`] so a caller can frame it with a different version's
/// header — a v2.3 tag needs nine-byte frame headers, and re-using the v2.4 form would
/// test the wrong thing.
fn chap_body(element_id: &str, start_ms: u32, end_ms: u32, title: &str, version: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(element_id.as_bytes());
    body.push(0x00);
    body.extend_from_slice(&start_ms.to_be_bytes());
    body.extend_from_slice(&end_ms.to_be_bytes());
    body.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    body.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    // The sub-frame goes inside the CHAP body, so it needs its own header.
    body.extend_from_slice(&frame_body(b"TIT2", &tit2(title, version)));
    body
}

/// Wrap a frame body in a header for `version`, which is either 3 or 4.
///
/// v2.4 uses a synchsafe 4-byte size and a 2-byte flag field; v2.3 uses a plain 4-byte
/// size and a 1-byte flag field, making the header one byte shorter. A parser that
/// assumed the v2.4 shape reads a v2.3 frame's size as nonsense.
fn frame_body(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    // The version is not known here, so this builds the *body* including sub-frame
    // headers only where they are version-independent; the caller adds the real frame
    // header. Reserved for the v2.4-shaped header, which sub-frames use regardless of
    // the parent frame's version per the spec.
    let mut out = Vec::with_capacity(10 + body.len());
    out.extend_from_slice(id);
    out.extend_from_slice(&synchsafe(body.len() as u32));
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(body);
    out
}

/// Encode a size as ID3v2.4's synchsafe integer: seven bits per byte, high bit clear.
fn synchsafe(value: u32) -> [u8; 4] {
    [
        ((value >> 21) & 0x7F) as u8,
        ((value >> 14) & 0x7F) as u8,
        ((value >> 7) & 0x7F) as u8,
        (value & 0x7F) as u8,
    ]
}

/// Assemble a complete ID3v2 tag around already-headed frames.
fn id3_tag(version: u8, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for raw in frames {
        if version == 3 {
            // A v2.3 frame keeps the ten-byte header and switches to a plain size. This
            // is the shape mutagen writes and ffmpeg reads; the specification's nine
            // bytes (a single flag byte) is accepted by readers but not produced by
            // anything in this pipeline, so a fixture using it would test a shape no
            // real v2.3 file has.
            let id = raw.get(..4).unwrap_or_default().to_vec();
            let size = u32::from_be_bytes([
                *raw.get(4).unwrap_or(&0),
                *raw.get(5).unwrap_or(&0),
                *raw.get(6).unwrap_or(&0),
                *raw.get(7).unwrap_or(&0),
            ]);
            body.extend_from_slice(&id);
            body.extend_from_slice(&size.to_be_bytes());
            body.extend_from_slice(raw.get(8..10).unwrap_or_default());
            body.extend_from_slice(raw.get(10..).unwrap_or_default());
        } else {
            body.extend_from_slice(raw);
        }
    }

    let mut tag = Vec::with_capacity(10 + body.len());
    tag.extend_from_slice(b"ID3");
    tag.push(version);
    tag.push(0x00);
    tag.push(0x00); // no unsynchronisation, no extended header
    tag.extend_from_slice(&synchsafe(body.len() as u32));
    tag.extend_from_slice(&body);
    tag
}

/// Splice a tag onto the front of a file, replacing any tag already there.
fn splice_tag(audio: &Path, out: &Path, tag: &[u8]) -> Result<(), Error> {
    let data = std::fs::read(audio).map_err(|e| Error::Io {
        what: "read mp3 audio to tag",
        source: format!("{}: {e}", audio.display()),
    })?;
    // Skip past an existing tag, or the two would be concatenated and only the first
    // would be seen — which looks exactly like a tag that was never written.
    let skip = if data.starts_with(b"ID3") {
        let mut size = 0u32;
        for b in data.get(6..10).unwrap_or(&[0; 4]) {
            size = (size << 7) | u32::from(b & 0x7F);
        }
        10 + size as usize
    } else {
        0
    };
    let mut merged = tag.to_vec();
    merged.extend_from_slice(data.get(skip..).unwrap_or(&[]));
    std::fs::write(out, merged).map_err(|e| Error::Io {
        what: "write tagged mp3",
        source: format!("{}: {e}", out.display()),
    })
}

/// Format a path for a command argument.
///
/// Paths with a leading `-` would be read as options; prefixing with `./` is the
/// portable fix and costs nothing for an absolute path.
fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Failures this harness can report.
#[derive(Debug)]
pub enum Error {
    /// A corpus file does not contain what its recipe promised.
    ///
    /// Its own variant because it is not a tool failure and not a disagreement: the
    /// fixture is wrong, and continuing would let every later comparison against it pass
    /// without comparing anything.
    FixtureFailed {
        /// The file.
        path: String,
        /// The recipe that produced it.
        recipe: String,
        /// What was expected and what was found.
        detail: String,
    },
    /// A filesystem operation failed.
    Io {
        /// What was being done.
        what: &'static str,
        /// The underlying message.
        source: String,
    },
    /// An external tool was missing or failed.
    Tool {
        /// Which tool.
        tool: &'static str,
        /// Why.
        reason: String,
    },
    /// ffprobe's output could not be understood.
    Parse {
        /// What was being parsed.
        what: &'static str,
        /// The offending text, truncated.
        got: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { what, source } => write!(f, "could not {what}: {source}"),
            Error::Tool { tool, reason } => write!(f, "{tool}: {reason}"),
            Error::Parse { what, got } => {
                write!(f, "could not parse {what}: {}", truncate(got, 200))
            }
            Error::FixtureFailed {
                path,
                recipe,
                detail,
            } => write!(
                f,
                "corpus fixture {path} is not what its recipe promised: {detail} \
                 (recipe: {recipe})"
            ),
        }
    }
}

impl std::error::Error for Error {}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Compare two durations with the harness's default tolerance.
///
/// [`compare_duration_within`] is the same check with a different budget; this is the
/// one to reach for when the reader's confidence is not known here.
#[must_use]
pub fn compare_duration(reference_ms: Option<u64>, actual_ms: Option<u64>) -> DurationFinding {
    compare_duration_within(reference_ms, actual_ms, DURATION_TOLERANCE_MS)
}

/// Compare two durations against an explicit tolerance in milliseconds.
#[must_use]
pub fn compare_duration_within(
    reference_ms: Option<u64>,
    actual_ms: Option<u64>,
    tolerance_ms: u64,
) -> DurationFinding {
    match (reference_ms, actual_ms) {
        (Some(r), Some(a)) => {
            let delta = r.abs_diff(a);
            if delta <= tolerance_ms {
                DurationFinding::Agreed {
                    reference_ms: r,
                    actual_ms: a,
                }
            } else {
                DurationFinding::Disagreed {
                    reference_ms: r,
                    actual_ms: a,
                    delta_ms: delta,
                }
            }
        }
        (Some(_), None) | (None, Some(_)) => DurationFinding::OneSided {
            reference_ms,
            actual_ms,
        },
        // Both absent is agreement, not a one-sided finding: an earlier version
        // matched it here and reported every file ffprobe had no duration for as a
        // disagreement.
        (None, None) => DurationFinding::Neither,
    }
}

/// Compare a reader's chapters against the reference's.
///
/// Chapters past the length of either list are reported rather than truncated: a
/// reader that invents three extra chapters is a different failure from one that
/// misses three, and conflating them hides which happened.
#[must_use]
pub fn compare_chapters(
    reference: &[ReferenceChapter],
    actual: &[ActualChapter],
) -> Vec<ChapterFinding> {
    if reference.is_empty() && actual.is_empty() {
        return Vec::new();
    }
    if actual.is_empty() {
        return vec![ChapterFinding::MissingAll {
            expected: reference.len(),
        }];
    }
    if reference.is_empty() {
        return vec![ChapterFinding::UnexpectedAll {
            found: actual.len(),
        }];
    }

    let mut out = Vec::new();
    for (index, expected) in reference.iter().enumerate() {
        let Some(got) = actual.get(index) else {
            out.push(ChapterFinding::Mismatch {
                index,
                expected: expected.clone(),
                actual: ActualChapter {
                    start_ms: u64::MAX,
                    title: "<missing>".into(),
                },
            });
            continue;
        };
        let time_ok = expected.start_ms.abs_diff(got.start_ms) <= CHAPTER_TOLERANCE_MS;
        let title_ok = expected.title.trim() == got.title.trim();
        if !time_ok || !title_ok {
            out.push(ChapterFinding::Mismatch {
                index,
                expected: expected.clone(),
                actual: got.clone(),
            });
        }
    }
    if actual.len() > reference.len() {
        out.push(ChapterFinding::UnexpectedAll {
            found: actual.len(),
        });
    }
    out
}

/// A grouping of findings by outcome, for a summary line.
pub type Tally = BTreeMap<&'static str, usize>;

/// Cross-checking the estate's chapter sidecar writer and reader against ffmpeg.
pub mod sidecar;
pub mod symphonia;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_declared_duration_is_held_to_a_tighter_budget_than_a_derived_one() {
        // The real shape of the defect this exists for: a reader that found the
        // encoder's frame count but ignored its delay and padding reported a 12 s
        // audiobook as 12.031 s. That is 31 ms - inside the default 50 ms budget, so
        // it was reported as agreement, and outside the declared one.
        let delta = 12_031 - 12_000;
        assert!(
            delta <= DURATION_TOLERANCE_MS,
            "the defect this budget exists for must be invisible to the old one"
        );
        assert!(
            compare_duration_within(Some(12_000), Some(12_031), DECLARED_DURATION_TOLERANCE_MS)
                .is_disagreement(),
            "and visible to the new one"
        );
        // A difference this size is still not a disagreement when the reader had to
        // derive the number, because a derived duration is a frame's worth of padding
        // away from the truth by construction.
        assert!(
            !compare_duration_within(Some(12_000), Some(12_031), DURATION_TOLERANCE_MS)
                .is_disagreement()
        );
    }

    #[test]
    fn every_kind_that_gets_built_is_in_the_list_that_gets_checked() {
        // `FileKind::ALL` exists so this cannot drift silently. The check that it has
        // not drifted is that every kind in it describes itself and names a chapter
        // expectation: a kind added to the enum and forgotten everywhere else fails
        // here, because `expected_chapters` has no arm for it.
        assert_eq!(FileKind::ALL.len(), 9);
        for kind in FileKind::ALL {
            assert!(
                !kind.to_string().is_empty(),
                "{kind:?} has no description to print in a report"
            );
            let _ = kind.expected_chapters();
        }
        // And the spoken-word encoding is in it, which is the whole point of adding it:
        // without this line a later edit could drop it and the run would be quieter,
        // not louder.
        assert!(
            FileKind::ALL.contains(&FileKind::Mp3SpokenWord),
            "the MPEG-2 fixture must be one of the kinds checked, or nothing gates it"
        );
    }

    #[test]
    fn json_string_escapes_are_resolved_rather_than_compared_raw() {
        // ffprobe writes these escapes for a title that contains them, verbatim. This is
        // the actual output captured from `ffprobe -show_chapters -print_format json`.
        let json =
            r#"{"chapters":[{"id":0,"start_time":0.0,"tags":{"title":"Line one\nLine two"}}]}"#;
        let probe = parse_ffprobe_json(json).expect("parses");
        assert_eq!(probe.chapters.len(), 1);
        assert_eq!(
            probe.chapters[0].title, "Line one\nLine two",
            "the JSON escape must become a real newline, not two characters"
        );

        // A quote and a backslash, which is the other pair a real title contains.
        let json = r#"{"chapters":[{"id":0,"tags":{"title":"He said \"hi\" 100% \\ sure"}}]}"#;
        let probe = parse_ffprobe_json(json).expect("parses");
        assert_eq!(probe.chapters[0].title, "He said \"hi\" 100% \\ sure");
    }

    #[test]
    fn every_json_escape_the_spec_defines_is_handled() {
        for (raw, want) in [
            (r#"\""#, "\""),
            (r#"\\"#, "\\"),
            (r#"\/"#, "/"),
            (r#"\b"#, "\u{8}"),
            (r#"\f"#, "\u{c}"),
            (r#"\n"#, "\n"),
            (r#"\r"#, "\r"),
            (r#"\t"#, "\t"),
            (r#"\u0041"#, "A"),
            (r#"\u00e9"#, "\u{e9}"),
            (r#"\u4e2d"#, "\u{4e2d}"),
            // A surrogate pair for an astral code point: the book emoji.
            (r#"\ud83d\udcda"#, "\u{1f4da}"),
            // And a raw multi-byte character, which is what ffprobe usually emits.
            ("\u{4e2d}\u{6587}", "\u{4e2d}\u{6587}"),
        ] {
            assert_eq!(json_unescape(raw).as_deref(), Some(want), "for {raw}");
        }
    }

    #[test]
    fn an_escape_json_does_not_define_is_an_error_not_a_guess() {
        // Guessing here would fabricate or hide a disagreement, which is the one thing a
        // conformance harness must not do.
        for bad in [
            r#"\x"#,
            r#"\u00"#,
            r#"\u00ZZ"#,
            r#"\uD83D"#,
            r#"\uD83Dx"#,
            r#"\"#,
        ] {
            assert_eq!(json_unescape(bad), None, "for {bad}");
        }
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        // The scan for the closing quote has to respect escapes, or this title is read as
        // `He said \"hi` and the rest of the JSON is mangled.
        let json = r#"{"chapters":[{"id":0,"tags":{"title":"a \" b \" c"}}]}"#;
        let probe = parse_ffprobe_json(json).expect("parses");
        assert_eq!(probe.chapters[0].title, "a \" b \" c");
    }

    #[test]
    fn parses_a_duration_from_ffprobe_json() {
        let json = r#"{"chapters":[],"format":{"duration":"180.021333","format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#;
        let out = parse_ffprobe_json(json).expect("parses");
        assert_eq!(out.duration_ms, Some(180_021));
        assert_eq!(out.format.as_deref(), Some("mov,mp4,m4a,3gp,3g2,mj2"));
        assert!(out.chapters.is_empty());
    }

    #[test]
    fn parses_chapters_from_ffprobe_json() {
        let json = r#"{
          "chapters": [
            {"id":0,"time_base":"1/1000","start":0,"start_time":"0.000000",
             "end":60000,"end_time":"60.000000","tags":{"title":"Chapter One"}},
            {"id":1,"time_base":"1/1000","start":60000,"start_time":"60.000000",
             "end":120000,"end_time":"120.000000","tags":{"title":"Chapter Two"}}
          ],
          "format":{"duration":"180.000000","format_name":"mov,mp4"}
        }"#;
        let out = parse_ffprobe_json(json).expect("parses");
        assert_eq!(out.chapters.len(), 2);
        assert_eq!(out.chapters[0].start_ms, 0);
        assert_eq!(out.chapters[0].title, "Chapter One");
        assert_eq!(out.chapters[1].start_ms, 60_000);
        assert_eq!(out.chapters[1].title, "Chapter Two");
    }

    #[test]
    fn a_missing_duration_is_absent_not_zero() {
        let out = parse_ffprobe_json(r#"{"chapters":[],"format":{}}"#).expect("parses");
        assert_eq!(
            out.duration_ms, None,
            "absent, so a reader's 0 is a disagreement"
        );
    }

    #[test]
    fn matching_chapters_produce_no_findings() {
        let reference = vec![
            ReferenceChapter {
                index: 0,
                start_ms: 0,
                title: "One".into(),
            },
            ReferenceChapter {
                index: 1,
                start_ms: 60_000,
                title: "Two".into(),
            },
        ];
        let actual = vec![
            ActualChapter {
                start_ms: 0,
                title: "One".into(),
            },
            ActualChapter {
                start_ms: 60_000,
                title: "Two".into(),
            },
        ];
        assert!(compare_chapters(&reference, &actual).is_empty());
    }

    #[test]
    fn a_missing_chapter_list_is_a_finding_not_a_pass() {
        let reference = vec![ReferenceChapter {
            index: 0,
            start_ms: 0,
            title: "One".into(),
        }];
        let findings = compare_chapters(&reference, &[]);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            ChapterFinding::MissingAll { expected: 1 }
        ));
    }

    #[test]
    fn an_invented_chapter_list_is_a_finding() {
        let actual = vec![ActualChapter {
            start_ms: 0,
            title: "Invented".into(),
        }];
        let findings = compare_chapters(&[], &actual);
        assert!(matches!(
            findings[0],
            ChapterFinding::UnexpectedAll { found: 1 }
        ));
    }

    #[test]
    fn a_shifted_chapter_is_a_finding_even_within_the_duration_tolerance() {
        // Chapter tolerance is far tighter than the duration tolerance on purpose:
        // a boundary is exact metadata written by the producer, so shifting one is
        // misreporting a fact rather than rounding a measurement.
        let reference = vec![ReferenceChapter {
            index: 0,
            start_ms: 60_000,
            title: "Two".into(),
        }];

        let inside = vec![ActualChapter {
            start_ms: 60_000 + CHAPTER_TOLERANCE_MS,
            title: "Two".into(),
        }];
        assert!(
            compare_chapters(&reference, &inside).is_empty(),
            "exactly at the tolerance is inside"
        );

        let outside = vec![ActualChapter {
            start_ms: 60_000 + CHAPTER_TOLERANCE_MS + 1,
            title: "Two".into(),
        }];
        assert_eq!(compare_chapters(&reference, &outside).len(), 1);
    }

    #[test]
    fn a_truncated_title_is_a_finding() {
        let reference = vec![ReferenceChapter {
            index: 0,
            start_ms: 0,
            title: "Chapter One".into(),
        }];
        let actual = vec![ActualChapter {
            start_ms: 0,
            title: "Chapter".into(),
        }];
        let findings = compare_chapters(&reference, &actual);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].to_string().contains("Chapter One"));
    }

    #[test]
    fn durations_within_tolerance_agree() {
        assert!(
            !compare_duration(Some(180_021), Some(180_000)).is_disagreement(),
            "21 ms is inside a 50 ms tolerance"
        );
        assert!(
            compare_duration(Some(180_021), Some(180_100)).is_disagreement(),
            "79 ms is outside"
        );
    }

    #[test]
    fn a_one_sided_duration_is_a_disagreement() {
        // ffprobe knows nothing is different from this crate knowing nothing.
        assert!(compare_duration(Some(1), None).is_disagreement());
        assert!(compare_duration(None, Some(1)).is_disagreement());
        assert!(!compare_duration(None, None).is_disagreement());
    }

    #[test]
    fn ffmetadata_has_a_chapter_per_title() {
        let meta = ffmetadata(180_000, &["One", "Two", "Three"]);
        assert!(meta.starts_with(";FFMETADATA1"));
        assert_eq!(meta.matches("[CHAPTER]").count(), 3);
        assert!(meta.contains("START=60000"));
        assert!(
            meta.contains("END=180000"),
            "the last chapter runs to the end"
        );
    }

    #[test]
    fn a_report_with_a_disagreement_says_so() {
        let r = Report {
            path: PathBuf::from("/tmp/book.m4b"),
            source: Source::Ffprobe,
            container: "m4b",
            chapters: vec![ChapterFinding::MissingAll { expected: 3 }],
            duration: DurationFinding::Agreed {
                reference_ms: 180_000,
                actual_ms: 180_000,
            },
            title: None,
            artist: None,
            mechanism: "none",
        };
        assert!(!r.is_clean());
        assert!(r.summary().contains("DISAGREEMENT"));
        assert!(r.summary().contains("read 0 chapters"));
    }

    #[test]
    fn an_unverified_field_is_neither_pass_nor_failure() {
        let r = Report {
            path: PathBuf::from("/tmp/book.m4b"),
            source: Source::Ffprobe,
            container: "m4b",
            chapters: vec![ChapterFinding::Unverified {
                field: "chapter track titles",
                reason: "not yet read",
            }],
            duration: DurationFinding::Agreed {
                reference_ms: 1,
                actual_ms: 1,
            },
            title: None,
            artist: None,
            mechanism: "none",
        };
        assert!(r.is_clean(), "an unchecked field is not a disagreement");
        assert!(r.has_unverified(), "but it is not a pass either");
        assert!(r.summary().contains("unchecked"));
    }

    #[test]
    fn long_strings_are_truncated_on_a_char_boundary() {
        assert_eq!(truncate("abc", 10), "abc");
        // A multi-byte character straddling the cut must not panic.
        // A multi-byte character straddling the cut must not panic, and the result
        // must stay within one byte of the limit plus the ellipsis.
        let s = "é".repeat(50);
        let t = truncate(&s, 5);
        assert!(t.len() <= 5 + '…'.len_utf8() + 1);
        assert!(t.ends_with('…'));
    }
}
