//! Cross-checking the estate's chapter sidecar writer and reader against ffmpeg.
//!
//! # The gap this closes
//!
//! The rest of this harness points every tool at files **ffmpeg** wrote, then compares
//! the estate's reading against ffprobe's. That catches a reader defect. It cannot
//! catch a *writer* defect, because the estate never writes anything: the only bytes
//! under test were produced by ffmpeg, so a chapter list this estate emits has never
//! been seen by an implementation that did not write it.
//!
//! That is the same blind spot that let `mp4-core` believe it could read `chpl` for
//! several releases \\u{2014} every `chpl` test had run against a box the crate wrote
//! itself. So this module runs the format both ways around, with ffmpeg as the
//! independent implementation on the far side of each:
//!
//! * **write** \\u{2014} `audiobook_core::sidecar` renders a `;FFMETADATA1` file, ffmpeg
//!   muxes it into an M4B, and ffprobe reports what survived. Anything ffmpeg drops or
//!   mangles is a fact about the estate's writer, not a rounding difference.
//! * **read** \\u{2014} ffmpeg is asked to dump a real corpus file back out as
//!   `;FFMETADATA1` (`-f ffmetadata`), and the estate parses ffmpeg's own output. A file
//!   this estate has never seen, produced by a writer that did not consult it.
//!
//! # Why the awkward titles
//!
//! Every case below is a title that a real book has and that a naive implementation
//! mangles. They are here because each one was found by running ffmpeg, not by reasoning
//! about the format: the `;`-and-`=` escapes work, and the one that does not is a
//! trailing backslash, which costs the file an entire chapter with no diagnostic at all.

use std::path::{Path, PathBuf};

use audiobook_core::sidecar::{self, SidecarChapters};

use crate::{args, fs_write, path_arg, run_capture, Error};

/// One awkward title and why it is here.
struct TitleCase {
    /// A short name for the report.
    name: &'static str,
    /// The title itself.
    title: &'static str,
    /// What this title is here to find out.
    why: &'static str,
}

/// The titles every run exercises.
const TITLES: &[TitleCase] = &[
    TitleCase {
        name: "equals",
        title: "Has = equals",
        why: "`=` is the key/value separator and must be escaped",
    },
    TitleCase {
        name: "semicolon",
        title: "Has ; semi",
        why: "`;` starts a comment and must be escaped",
    },
    TitleCase {
        name: "hash",
        title: "Has # hash",
        why: "`#` starts a comment and must be escaped",
    },
    TitleCase {
        name: "backslash-mid",
        title: "Has \\ backslash inside",
        why: "a backslash mid-title needs no continuation, and must not get one",
    },
    TitleCase {
        name: "newline",
        title: "Line one\nLine two",
        why: "a newline is escaped as a backslash and a real newline",
    },
    TitleCase {
        name: "leading-space",
        title: " Leading and trailing ",
        why: "ffmpeg keeps whitespace in a value; trimming it would be a defect",
    },
    TitleCase {
        name: "looks-like-header",
        title: "Looks like a header [CHAPTER]",
        why: "a title that would be a section header if it were not escaped",
    },
    TitleCase {
        name: "unicode",
        title: "Unicode: \u{e9}\u{e8} \u{4e2d}\u{6587} \u{1f4da}",
        why: "multi-byte UTF-8, which a byte-oriented writer can truncate",
    },
    TitleCase {
        name: "quotes-and-percent",
        title: "He said \"hi\" 100% sure",
        why: "quotes and percent are not special, and must not be mangled anyway",
    },
    TitleCase {
        name: "empty",
        title: "",
        why: "an untitled boundary is still a boundary and must survive",
    },
];

/// The one title ffmpeg provably cannot read back.
///
/// Kept in its own case rather than mixed into [`TITLES`] so the harness can say *which*
/// divergence is ffmpeg's limitation and which, if any, is the estate's.
const LOSSLESS_TO_FFMPEG: &str = "Ends with a backslash \\";

/// How closely two sidecar timestamps must agree, in milliseconds.
const SIDE_TOLERANCE_MS: u64 = 2;

/// One chapter as one side reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// When the chapter starts, in milliseconds.
    pub start_ms: u64,
    /// The chapter's title.
    pub title: String,
}

/// A way the two sides failed to agree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SidecarFinding {
    /// The estate's own writer and reader disagreed, which is a defect with no excuse.
    SelfRoundTrip {
        /// Which case.
        case: &'static str,
        /// What differed.
        detail: String,
    },
    /// ffmpeg and the estate disagreed about the same bytes.
    ///
    /// Not automatically a bug in either: see [`SidecarFinding::DocumentedFfmpegLimit`]
    /// for the one divergence that is known to be ffmpeg's.
    Disagreement {
        /// Which case.
        case: &'static str,
        /// The chapter index.
        index: usize,
        /// What the estate emitted.
        ours: String,
        /// What ffmpeg reported.
        theirs: String,
    },
    /// A divergence already understood and attributed to ffmpeg.
    ///
    /// Reported rather than passed silently, and still counted separately from an
    /// agreement so a reader of the output cannot mistake it for one.
    DocumentedFfmpegLimit {
        /// Which case.
        case: &'static str,
        /// What ffmpeg cannot do.
        detail: String,
    },
    /// ffmpeg refused the input outright.
    MuxFailed {
        /// Which case.
        case: &'static str,
        /// Its own words.
        detail: String,
    },
    /// A field this harness does not compare, stated rather than assumed.
    Unchecked {
        /// What is unchecked.
        field: &'static str,
        /// Why it is not compared.
        why: &'static str,
    },
}

/// What one direction of the round trip found.
#[derive(Debug, Clone)]
pub struct SidecarReport {
    /// Whether ffmpeg could be run at all.
    pub ffmpeg_available: bool,
    /// One report per case.
    pub cases: Vec<SidecarCase>,
    /// Everything that is not an agreement.
    pub findings: Vec<SidecarFinding>,
}

/// One case's result.
#[derive(Debug, Clone)]
pub struct SidecarCase {
    /// The case name.
    pub name: &'static str,
    /// Why the case exists.
    pub why: &'static str,
    /// What the estate emitted, as the estate re-read it.
    pub ours: Vec<Reading>,
    /// What ffprobe reported after ffmpeg muxed it, when ffmpeg was available.
    pub reference: Option<Vec<Reading>>,
}

impl SidecarReport {
    /// Whether nothing unexplained went wrong.
    ///
    /// A [`SidecarFinding::DocumentedFfmpegLimit`] and a
    /// [`SidecarFinding::Unchecked`] do **not** make a report unclean, because neither is
    /// a defect in this estate: one is a limitation already attributed to ffmpeg, the
    /// other is a comparison that did not happen. Both are still printed, and
    /// [`SidecarReport::documented_limits`] counts the first, so "clean" never means
    /// "nothing to say".
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.findings.iter().any(|f| {
            matches!(
                f,
                SidecarFinding::Disagreement { .. }
                    | SidecarFinding::SelfRoundTrip { .. }
                    | SidecarFinding::MuxFailed { .. }
            )
        })
    }

    /// Whether ffmpeg could not represent something this estate can.
    #[must_use]
    pub fn documented_limits(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f, SidecarFinding::DocumentedFfmpegLimit { .. }))
            .count()
    }

    /// Whether anything the estate emitted failed to survive its own round trip.
    #[must_use]
    pub fn has_self_defect(&self) -> bool {
        self.findings
            .iter()
            .any(|f| matches!(f, SidecarFinding::SelfRoundTrip { .. }))
    }

    /// A one-line description for each case.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = format!(
            "sidecar round trip: {} case(s), ffmpeg {}\n",
            self.cases.len(),
            if self.ffmpeg_available {
                "compared"
            } else {
                "UNAVAILABLE"
            }
        );
        for case in &self.cases {
            let verdict = match &case.reference {
                Some(reference) if reference == &case.ours => "agree".to_string(),
                Some(reference) => format!(
                    "DIFFER (ours {}, ffmpeg {})",
                    case.ours.len(),
                    reference.len()
                ),
                None => "not compared".to_string(),
            };
            out.push_str(&format!(
                "  {:<20} {:<14} {} chapter(s)  {}\n",
                case.name,
                verdict,
                case.ours.len(),
                case.why
            ));
        }
        if self.documented_limits() > 0 {
            out.push_str(&format!(
                "\n  {} divergence(s) attributed to ffmpeg, reported not hidden",
                self.documented_limits()
            ));
        }
        if !self.ffmpeg_available {
            out.push_str(
                "\n\n  ffmpeg was not available, so nothing was compared against an \
                 independent implementation.\n  Every case below is a self-check, and a \
                 self-check against\n  our own writer proves nothing about whether ffmpeg \
                 would agree.",
            );
        }
        out
    }
}

/// Run both directions of the sidecar round trip under `root`.
///
/// # Errors
///
/// Returns an error when the corpus source audio cannot be written. A missing ffmpeg is
/// **not** an error: it is reported as an unchecked comparison, because "ffmpeg was absent"
/// and "ffmpeg agreed" must never look alike.
pub fn check(root: &Path) -> Result<SidecarReport, Error> {
    let source = root.join("source.wav");
    if !source.exists() {
        return Err(Error::Io {
            what: "sidecar check needs the corpus source audio; generate the corpus first",
            source: source.display().to_string(),
        });
    }

    let mut cases: Vec<SidecarCase> = Vec::new();
    let mut findings: Vec<SidecarFinding> = Vec::new();
    let ffmpeg_available = crate::has_ffmpeg() && crate::has_ffprobe();

    for case in TITLES {
        let titles = [case.title];
        let chapters = SidecarChapters {
            chapters: titles
                .iter()
                .enumerate()
                .map(|(i, t)| audiobook_core::Chapter::new(t, i as u64 * 1_000, 0))
                .collect(),
            ..SidecarChapters::default()
        };
        let (ours, reference) = one_case(root, &source, case, &chapters, ffmpeg_available);
        compare(
            &mut findings,
            case,
            &ours,
            reference.as_deref(),
            ffmpeg_available,
        );
        cases.push(SidecarCase {
            name: case.name,
            why: case.why,
            ours,
            reference,
        });
    }

    // The trailing-backslash case, on its own so the attribution is unambiguous.
    let hazard = TitleCase {
        name: "trailing-backslash",
        title: LOSSLESS_TO_FFMPEG,
        why: "the one title ffmpeg cannot read back",
    };
    let hazardous = SidecarChapters {
        chapters: vec![
            audiobook_core::Chapter::new(LOSSLESS_TO_FFMPEG, 0, 0),
            audiobook_core::Chapter::new("Second", 1_000, 0),
            audiobook_core::Chapter::new("Third", 2_000, 0),
        ],
        ..SidecarChapters::default()
    };
    let (ours, reference) = one_case(root, &source, &hazard, &hazardous, ffmpeg_available);

    // The estate's own round trip must be exact. This is the one place where a difference
    // is unambiguously a defect rather than a disagreement between implementations.
    let expected: Vec<Reading> = hazardous
        .chapters
        .iter()
        .map(|c| Reading {
            start_ms: c.start_ms,
            title: c.title.clone(),
        })
        .collect();
    if ours != expected {
        findings.push(SidecarFinding::SelfRoundTrip {
            case: hazard.name,
            detail: format!("emitted {ours:?} but read back {ours:?}", ours = ours),
        });
    }

    match (&reference, ffmpeg_available) {
        (Some(reference), true) => {
            // ffmpeg loses a chapter here. That was verified directly and is a property
            // of its reader, so it is attributed rather than counted as our defect \\u{2014}
            // but it is reported, because "we agree" would be false.
            if reference.len() != expected.len() {
                findings.push(SidecarFinding::DocumentedFfmpegLimit {
                    case: hazard.name,
                    detail: format!(
                        "a title ending in a backslash is escaped correctly as `\\\\`, but \
                         ffmpeg's reader takes that as a line continuation and swallows \
                         the next `[CHAPTER]` header: {} chapters in, {} out, and \
                         nothing on stderr. This crate counts backslashes and keeps all \
                         {}.",
                        expected.len(),
                        reference.len(),
                        expected.len()
                    ),
                });
            } else {
                compare(
                    &mut findings,
                    &hazard,
                    &ours,
                    Some(reference),
                    ffmpeg_available,
                );
            }
            // The survivor's timestamp is wrong too, which is worth its own line: a
            // chapter count that merely drops is a smaller claim than a chapter that
            // reports the wrong time.
            if let Some(first) = reference.first() {
                if reference.len() < expected.len() && first.start_ms != expected[0].start_ms {
                    findings.push(SidecarFinding::DocumentedFfmpegLimit {
                        case: hazard.name,
                        detail: format!(
                            "the surviving chapter reports {} ms where the input said {} ms, \
                             because the out-of-order `START` is discarded rather than \
                             applied",
                            first.start_ms, expected[0].start_ms
                        ),
                    });
                }
            }
        }
        (_, false) => {
            findings.push(SidecarFinding::Unchecked {
                field: "trailing-backslash through ffmpeg",
                why: "ffmpeg or ffprobe is not on PATH",
            });
        }
        (None, true) => {
            findings.push(SidecarFinding::MuxFailed {
                case: hazard.name,
                detail: "ffmpeg produced no chapters to read back".into(),
            });
        }
    }

    cases.push(SidecarCase {
        name: hazard.name,
        why: hazard.why,
        ours,
        reference,
    });

    // The reverse direction: ffmpeg dumps a real corpus file as `;FFMETADATA1` and the
    // estate reads ffmpeg's own output. This is the analogue of reading a real `chpl`
    // rather than one this estate wrote.
    let book = root.join("book.m4b");
    if ffmpeg_available && book.exists() {
        read_direction(&book, &mut findings);
    } else {
        findings.push(SidecarFinding::Unchecked {
            field: "ffmpeg's own ffmetadata output",
            why: "ffmpeg is unavailable or the corpus book is missing",
        });
    }

    // Fields this harness still does not compare, said out loud.
    findings.push(SidecarFinding::Unchecked {
        field: "MP4Box's `CHAPTER01` form",
        why: "MP4Box is not available, so the format is only round-tripped through our \
              own reader",
    });
    findings.push(SidecarFinding::Unchecked {
        field: "ZoomPlayer and SMPTE forms against a real consumer",
        why: "no independent reader for either was available",
    });

    Ok(SidecarReport {
        ffmpeg_available,
        cases,
        findings,
    })
}

/// Render one case, re-read it with the estate, and mux it through ffmpeg.
fn one_case(
    root: &Path,
    source: &Path,
    case: &TitleCase,
    chapters: &SidecarChapters,
    ffmpeg_available: bool,
) -> (Vec<Reading>, Option<Vec<Reading>>) {
    let rendered = chapters.to_ffmetadata();

    // The estate reading its own output. Wrong here means the crate is lossy in both
    // directions, which needs no external tool to detect and never has an excuse.
    let ours = match sidecar::parse(&rendered) {
        Ok(parsed) => parsed
            .chapters
            .iter()
            .map(|c| Reading {
                start_ms: c.start_ms,
                title: c.title.clone(),
            })
            .collect(),
        Err(e) => vec![Reading {
            start_ms: 0,
            title: format!("UNPARSEABLE: {e}"),
        }],
    };

    if !ffmpeg_available {
        return (ours, None);
    }

    let meta = root.join(format!("sidecar-{}.ffmeta", case.name));
    let out = root.join(format!("sidecar-{}.m4b", case.name));
    if fs_write(&meta, &rendered).is_err() {
        return (ours, None);
    }
    if run_capture(
        "ffmpeg",
        &args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            &path_arg(source),
            "-i",
            &path_arg(&meta),
            "-map_metadata",
            "1",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
            &path_arg(&out),
        ]),
    )
    .is_err()
    {
        return (ours, None);
    }
    let reference = crate::ffprobe(&out).ok().map(|p| {
        p.chapters
            .into_iter()
            .map(|c| Reading {
                start_ms: c.start_ms,
                title: c.title,
            })
            .collect()
    });
    (ours, reference)
}

/// Compare one case's two readings.
fn compare(
    findings: &mut Vec<SidecarFinding>,
    case: &TitleCase,
    ours: &[Reading],
    reference: Option<&[Reading]>,
    ffmpeg_available: bool,
) {
    let Some(reference) = reference else {
        if ffmpeg_available {
            findings.push(SidecarFinding::MuxFailed {
                case: case.name,
                detail: "ffmpeg produced nothing to read back".into(),
            });
        } else {
            findings.push(SidecarFinding::Unchecked {
                field: "sidecar round trip through ffmpeg",
                why: "ffmpeg or ffprobe is not on PATH",
            });
        }
        return;
    };

    if ours.len() != reference.len() {
        findings.push(SidecarFinding::Disagreement {
            case: case.name,
            index: usize::MAX,
            ours: format!("{} chapter(s)", ours.len()),
            theirs: format!("{} chapter(s)", reference.len()),
        });
        return;
    }
    for (i, (mine, theirs)) in ours.iter().zip(reference).enumerate() {
        if mine.start_ms.abs_diff(theirs.start_ms) > SIDE_TOLERANCE_MS {
            findings.push(SidecarFinding::Disagreement {
                case: case.name,
                index: i,
                ours: format!("start {} ms", mine.start_ms),
                theirs: format!("start {} ms", theirs.start_ms),
            });
        }
        if mine.title != theirs.title {
            findings.push(SidecarFinding::Disagreement {
                case: case.name,
                index: i,
                ours: format!("title {:?}", mine.title),
                theirs: format!("title {:?}", theirs.title),
            });
        }
    }
}

/// Have ffmpeg dump a real file as `;FFMETADATA1`, then read that back with the estate.
fn read_direction(book: &Path, findings: &mut Vec<SidecarFinding>) {
    let Ok(dumped) = run_capture(
        "ffmpeg",
        &args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &path_arg(book),
            "-map_chapters",
            "0",
            "-c:a",
            "copy",
            "-f",
            "ffmetadata",
            "-",
        ]),
    ) else {
        findings.push(SidecarFinding::Unchecked {
            field: "ffmpeg's own ffmetadata output",
            why: "ffmpeg refused to dump the corpus file",
        });
        return;
    };

    let text = String::from_utf8_lossy(&dumped);
    let Ok(parsed) = sidecar::parse(&text) else {
        findings.push(SidecarFinding::Disagreement {
            case: "read-direction",
            index: 0,
            ours: "could not parse ffmpeg's own output".into(),
            theirs: truncate(&text, 120),
        });
        return;
    };

    // ffprobe read the same M4B directly; the estate read ffmpeg's transcription of it.
    // Three paths to the same chapters, so a disagreement here localises the fault.
    let Ok(probe) = crate::ffprobe(book) else {
        findings.push(SidecarFinding::Unchecked {
            field: "read-direction cross-check",
            why: "ffprobe could not read the corpus file back",
        });
        return;
    };

    if parsed.chapters.len() != probe.chapters.len() {
        findings.push(SidecarFinding::Disagreement {
            case: "read-direction",
            index: usize::MAX,
            ours: format!("{} chapter(s) from ffmpeg's dump", parsed.chapters.len()),
            theirs: format!("{} chapter(s) from ffprobe", probe.chapters.len()),
        });
        return;
    }
    for (i, (mine, theirs)) in parsed.chapters.iter().zip(&probe.chapters).enumerate() {
        // ffmpeg writes the *audio* timebase into its dump, so the numbers are not
        // milliseconds and the comparison has to go through the same conversion. The
        // tolerance is the same 2 ms the file-level check uses.
        if mine.start_ms.abs_diff(theirs.start_ms) > SIDE_TOLERANCE_MS {
            findings.push(SidecarFinding::Disagreement {
                case: "read-direction",
                index: i,
                ours: format!("start {} ms", mine.start_ms),
                theirs: format!("start {} ms", theirs.start_ms),
            });
        }
        if mine.title != theirs.title {
            findings.push(SidecarFinding::Disagreement {
                case: "read-direction",
                index: i,
                ours: format!("title {:?}", mine.title),
                theirs: format!("title {:?}", theirs.title),
            });
        }
    }
}

/// Truncate for a report line.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &s[..end])
}

/// Where a sidecar check writes its scratch files.
pub fn scratch_dir(root: &Path) -> PathBuf {
    root.join("sidecar")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;

    #[test]
    fn every_title_is_distinct() {
        // A duplicate would make one case silently cover another and quietly weaken the
        // whole set.
        for (i, a) in TITLES.iter().enumerate() {
            for b in TITLES.iter().skip(i + 1) {
                assert_ne!(
                    a.title, b.title,
                    "`{}` and `{}` are the same title",
                    a.name, b.name
                );
                assert_ne!(a.name, b.name);
            }
        }
    }

    #[test]
    fn the_titles_include_every_character_the_format_reserves() {
        let all: String = TITLES.iter().map(|t| t.title).collect();
        for reserved in ['=', ';', '#', '\\', '\n'] {
            assert!(
                all.contains(reserved),
                "no case covers `{reserved}`, so its escaping is untested"
            );
        }
        assert!(
            all.chars().any(|c| c.len_utf8() > 1),
            "no case covers multi-byte UTF-8"
        );
    }

    #[test]
    fn the_hazardous_title_is_exactly_the_one_ffmpeg_cannot_read() {
        // If this stops being true the documented limitation is stale, and worse, the
        // harness would be attributing a defect to ffmpeg that ffmpeg no longer has.
        assert!(LOSSLESS_TO_FFMPEG.ends_with('\\'));
        assert!(!sidecar::ffmpeg_reads_back(LOSSLESS_TO_FFMPEG));
        for case in TITLES {
            assert!(
                sidecar::ffmpeg_reads_back(case.title),
                "`{}` should be readable by ffmpeg; if it no longer is, move it into its \
                 own case with its own explanation",
                case.name
            );
        }
    }

    #[test]
    fn a_report_with_only_documented_limits_is_clean() {
        // Documented ffmpeg limits are real findings about the ecosystem, not defects in
        // this estate, so they do not make the report unclean. Everything else does.
        let report = SidecarReport {
            ffmpeg_available: true,
            cases: Vec::new(),
            findings: vec![SidecarFinding::DocumentedFfmpegLimit {
                case: "trailing-backslash",
                detail: "known".into(),
            }],
        };
        assert!(report.is_clean());
        assert_eq!(report.documented_limits(), 1);
        assert!(!report.has_self_defect());

        let with_unchecked = SidecarReport {
            ffmpeg_available: false,
            cases: Vec::new(),
            findings: vec![SidecarFinding::Unchecked {
                field: "x",
                why: "y",
            }],
        };
        assert!(with_unchecked.is_clean());
        assert!(with_unchecked.summary().contains("UNAVAILABLE"));
        assert!(
            with_unchecked.summary().contains("proves nothing"),
            "an absent oracle must not read as an agreement"
        );
    }

    #[test]
    fn a_self_defect_or_disagreement_makes_the_report_unclean() {
        for finding in [
            SidecarFinding::SelfRoundTrip {
                case: "c",
                detail: "d".into(),
            },
            SidecarFinding::Disagreement {
                case: "c",
                index: 0,
                ours: "a".into(),
                theirs: "b".into(),
            },
            SidecarFinding::MuxFailed {
                case: "c",
                detail: "d".into(),
            },
        ] {
            let report = SidecarReport {
                ffmpeg_available: true,
                cases: Vec::new(),
                findings: vec![finding],
            };
            assert!(
                !report.is_clean(),
                "{:?} should be unclean",
                report.findings
            );
        }
    }

    #[test]
    fn comparing_two_readings_reports_a_count_difference_not_a_panic() {
        let mut findings = Vec::new();
        let ours = vec![Reading {
            start_ms: 0,
            title: "a".into(),
        }];
        let theirs = vec![Reading {
            start_ms: 0,
            title: "a".into(),
        }];
        compare(&mut findings, &TITLES[0], &ours, Some(&theirs), true);
        assert!(findings.is_empty(), "identical readings agree");

        // A different count must not index past the end of either side.
        let mut findings = Vec::new();
        compare(&mut findings, &TITLES[0], &ours, Some(&[]), true);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            SidecarFinding::Disagreement {
                index: usize::MAX,
                ..
            }
        ));
    }

    #[test]
    fn comparing_reports_a_timestamp_and_a_title_difference_separately() {
        let ours = vec![Reading {
            start_ms: 0,
            title: "a".into(),
        }];
        let theirs = vec![Reading {
            start_ms: 5_000,
            title: "b".into(),
        }];
        let mut findings = Vec::new();
        compare(&mut findings, &TITLES[0], &ours, Some(&theirs), true);
        assert_eq!(findings.len(), 2, "both the time and the title differ");
        assert!(findings.iter().any(|f| matches!(
            f,
            SidecarFinding::Disagreement { ours, .. } if ours.contains("start")
        )));
        assert!(findings.iter().any(|f| matches!(
            f,
            SidecarFinding::Disagreement { ours, .. } if ours.contains("title")
        )));
    }

    #[test]
    fn a_difference_inside_the_tolerance_is_not_a_finding() {
        let ours = vec![Reading {
            start_ms: 1_000,
            title: "a".into(),
        }];
        // Within 2 ms: this is rounding, not a reader shifting a boundary.
        let theirs = vec![Reading {
            start_ms: 1_002,
            title: "a".into(),
        }];
        let mut findings = Vec::new();
        compare(&mut findings, &TITLES[0], &ours, Some(&theirs), true);
        assert!(findings.is_empty());
    }

    #[test]
    fn a_missing_oracle_is_unchecked_rather_than_a_pass() {
        let ours = vec![Reading {
            start_ms: 0,
            title: "a".into(),
        }];
        let mut findings = Vec::new();
        compare(&mut findings, &TITLES[0], &ours, None, false);
        assert!(
            matches!(findings[0], SidecarFinding::Unchecked { .. }),
            "got {:?}",
            findings
        );
        // And when ffmpeg was expected but produced nothing, that is a failure, not an
        // unchecked field.
        let mut findings = Vec::new();
        compare(&mut findings, &TITLES[0], &ours, None, true);
        assert!(matches!(findings[0], SidecarFinding::MuxFailed { .. }));
    }

    #[test]
    fn truncate_is_length_safe() {
        assert_eq!(truncate("abc", 10), "abc");
        let t = truncate(&"\u{e9}".repeat(40), 5);
        assert!(t.ends_with('\u{2026}'));
        assert!(t.len() <= 5 + '\u{2026}'.len_utf8() + 2);
    }
}
