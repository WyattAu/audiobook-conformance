//! `audiobook-conformance` — read a corpus two ways and report every disagreement.
//!
//! Generation, comparison and reporting live in the library; this binary is the
//! thing a developer or CI job runs.

use std::path::PathBuf;
use std::process::ExitCode;

use audiobook_conformance::{
    compare_chapters, compare_duration, has_ffmpeg, has_ffprobe, ActualChapter, ChapterFinding,
    Corpus, CorpusFile, Error, FileKind, Report, Source,
};

fn main() -> ExitCode {
    let opts = match Options::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("audiobook-conformance: {e}");
            eprintln!("try --help");
            return ExitCode::from(2);
        }
    };
    if opts.help {
        print!("{}", Options::USAGE);
        return ExitCode::SUCCESS;
    }

    if !has_ffmpeg() || !has_ffprobe() {
        eprintln!(
            "audiobook-conformance: needs ffmpeg and ffprobe on PATH \
             (ffmpeg: {}, ffprobe: {})",
            if has_ffmpeg() { "yes" } else { "no" },
            if has_ffprobe() { "yes" } else { "no" }
        );
        eprintln!(
            "Without them there is no independent writer to generate a corpus and no \
             independent reader to check against, and a harness that verifies the code \
             against itself proves nothing."
        );
        return ExitCode::from(3);
    }

    match run(&opts) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("audiobook-conformance: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(opts: &Options) -> Result<ExitCode, Error> {
    println!("── corpus ──────────────────────────────────────────────────────");
    println!("generating in {}", opts.root.display());
    let corpus = Corpus::generate(&opts.root)?;
    println!("{} files generated\n", corpus.files.len());

    let mut reports: Vec<Report> = Vec::new();
    for file in &corpus.files {
        let report = check(file)?;
        if opts.verbose || !report.is_clean() || report.has_unverified() {
            println!("{}", report.summary());
        }
        if opts.verbose {
            println!("         mechanism: {}", report.mechanism);
        }
        reports.push(report);
    }

    let clean = reports.iter().filter(|r| r.is_clean()).count();
    let unverified = reports.iter().filter(|r| r.has_unverified()).count();
    let broken = reports.len() - clean;

    println!("\n── summary ────────────────────────────────────────────────────");
    for kind in ALL_KINDS {
        let of_kind: Vec<_> = reports
            .iter()
            .zip(corpus.files.iter())
            .filter(|(_, f)| f.kind == kind)
            .map(|(r, _)| r)
            .collect();
        if of_kind.is_empty() {
            continue;
        }
        let ok = of_kind.iter().filter(|r| r.is_clean()).count();
        let mark = if ok == of_kind.len() { "✓" } else { "✗" };
        println!(
            "{mark} {kind:<44} {ok}/{} agree with ffprobe",
            of_kind.len()
        );
    }
    println!("\n{clean} agree, {broken} disagree, {unverified} carry an unchecked field");

    if broken > 0 {
        println!(
            "\nEach disagreement above is a real finding: the estate crates and \
             ffprobe read the same file differently."
        );
        return Ok(ExitCode::from(1));
    }
    if unverified > 0 {
        println!(
            "\nNothing disagrees, but some fields are not compared yet. A pass here \
             means agreement on what is checked, not that everything is checked."
        );
    }
    Ok(ExitCode::SUCCESS)
}

const ALL_KINDS: [FileKind; 5] = [
    FileKind::M4bWithChapters,
    FileKind::M4aBare,
    FileKind::Mp3CbrTagged,
    FileKind::Mp3Vbr,
    FileKind::Mp3Folder,
];

/// Read one file with the estate crates and compare against ffprobe's report.
fn check(file: &CorpusFile) -> Result<Report, Error> {
    let bytes = std::fs::read(&file.path).map_err(|e| Error::Io {
        what: "read corpus file",
        source: format!("{}: {e}", file.path.display()),
    })?;
    // One probe for every kind. The distinction the harness cares about is the
    // *reference* it compares against, not which reader ran — `MediaProbe` detects the
    // container itself, which is itself part of what is being tested.
    let probe = audiobook_core::MediaProbe::probe(&bytes);
    let container = match file.kind {
        FileKind::M4bWithChapters | FileKind::M4aBare | FileKind::M4bTextTrackChapters => "mp4",
        FileKind::Mp3CbrTagged | FileKind::Mp3Vbr | FileKind::Mp3Folder => "mp3",
    };
    let chapters = probe.chapters.clone();
    let duration_ms = probe.duration_ms;
    let title = probe.title.clone();
    let artist = probe.artist.clone();

    // A file carrying both chapter mechanisms and disagreeing between them is a finding
    // in its own right: a library that silently prefers one shows a list the file
    // contradicts, and neither list is wrong on its own — so a plain chapter
    // comparison would pass while the file is internally inconsistent.
    let disagreement = probe.chapter_disagreement.clone();

    let actual: Vec<ActualChapter> = chapters
        .iter()
        .map(|(start_ms, t)| ActualChapter {
            start_ms: *start_ms,
            title: t.clone(),
        })
        .collect();

    let mut findings = compare_chapters(&file.reference_chapters, &actual);

    // A file carrying both chapter mechanisms and disagreeing between them is a finding
    // in its own right: a library that silently prefers one shows a list the file
    // contradicts, so the harness checks that the two agree rather than assuming it.
    if let Some(split) = &disagreement {
        findings.push(ChapterFinding::MechanismDisagreement {
            chpl_len: split.chpl.len(),
            track_len: split.chapter_track.len(),
        });
    }

    // Which mechanism supplied the list. `MediaProbe` prefers the QuickTime chapter
    // track, so an M4B written by ffmpeg is read through the track — not through the
    // `chpl` box the corpus's chapters were originally written into.
    let mechanism = match mp4_core::Mp4File::parse(&bytes) {
        Ok(f) if !f.chapter_track_chapters.chapters.is_empty() => "quicktime chapter track",
        Ok(f) if !f.chpl_chapters.chapters.is_empty() => "nero chpl",
        Ok(_) if !chapters.is_empty() => "container tags (CHAP/CTOC)",
        _ => "none",
    };

    Ok(Report {
        path: file.path.clone(),
        source: Source::Ffprobe,
        container,
        mechanism,
        chapters: findings,
        duration: compare_duration(file.reference_duration_ms, duration_ms),
        title,
        artist,
    })
}

struct Options {
    root: PathBuf,
    verbose: bool,
    help: bool,
}

impl Options {
    const USAGE: &'static str = "\
audiobook-conformance — cross-check the WyattAu audiobook stack against ffprobe

USAGE:
    audiobook-conformance [OPTIONS]

OPTIONS:
    --root <DIR>    Where to generate the corpus (default: /tmp/audiobook-corpus)
    -v, --verbose   Print a line for every file, not just the interesting ones
    -h, --help      Show this message

WHAT IT DOES
    Generates a corpus with ffmpeg — an independent writer — reads it back with
    mp4-core, id3-core and audiobook-core, and compares the result against
    ffprobe's own report of the same file. Exits non-zero when they disagree.

    A field this harness does not compare is reported as unchecked rather than
    counted as a pass.

REQUIRES
    ffmpeg and ffprobe on PATH. Without an independent writer and an independent
    reader there is nothing to check against, and the harness would be verifying
    the code against itself.
";

    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut root = PathBuf::from("/tmp/audiobook-corpus");
        let mut verbose = false;
        let mut help = false;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-v" | "--verbose" => verbose = true,
                "-h" | "--help" => help = true,
                "--root" => {
                    root = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or_else(|| "--root needs a directory".to_string())?;
                }
                other => return Err(format!("unrecognised argument `{other}`")),
            }
        }
        Ok(Self {
            root,
            verbose,
            help,
        })
    }
}
