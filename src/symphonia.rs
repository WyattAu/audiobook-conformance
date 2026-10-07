//! Symphonia as a third opinion on what a file is.
//!
//! # What this is, and what it is not
//!
//! Symphonia is a pure-Rust demuxer with no shared code and no shared history with either
//! ffmpeg or this estate, so it breaks ties: where the estate and ffmpeg agree, there is no
//! way to tell agreement from a shared misreading of a specification, and a third
//! independent implementation is the only way to find out.
//!
//! It provides **container identification** — what kind of file this is — and that is the
//! check worth having, because `audiobook-core::container::detect` classifies by signature
//! and a false positive there sends every later read down the wrong path.
//!
//! It does **not** provide duration, despite this module originally being written to
//! compare durations. Symphonia 0.6 exposes no duration without decoding: `Track`'s codec
//! parameters carry a sample rate and a codec id but no frame count, and neither the MP3
//! nor the ISOMP4 reader populates one. The first version of this assumed otherwise and
//! returned `None` for every file, which would have made a duration comparison pass
//! vacuously — both sides reporting nothing. Duration remains ffprobe's job, and the
//! reason is written down here so nobody re-derives it.

use std::path::Path;

use symphonia::core::formats::{probe::Hint, FormatOptions};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
// The registry of formats compiled into this build, which is what `probe` needs: a
// `Probe` on its own knows no formats at all.
use symphonia::default::register_enabled_formats;

/// What Symphonia identifies a file as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymphoniaReading {
    /// The name Symphonia gives the container, such as `mp3` or `iso mp4`.
    pub format_name: String,
}

/// Identify a file with Symphonia's probe.
///
/// `None` means Symphonia could not identify the file at all — which is a different fact
/// from identifying it as something unexpected, and the distinction matters: the first
/// says the file is not audio, the second says this estate's detector disagrees with
/// another implementation about what it is.
#[allow(clippy::expect_used, clippy::panic)]
pub fn read(path: &Path) -> Option<SymphoniaReading> {
    let source = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    // A hint from the extension helps the probe on files whose first bytes are ambiguous,
    // which is exactly the MP3-with-a-weird-header case.
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    let mut probe = Default::default();
    register_enabled_formats(&mut probe);
    // Ownership, not references: the probe takes the options by value and hands back a
    // boxed reader.
    let probed = probe
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    // `ProbedMetadata` dereferences to the `FormatReader` it wrapped, whose `FormatInfo`
    // carries a short name (`mp3`) and a long one (`MPEG audio`). The short one is what a
    // comparison wants: it is stable and it is what a person would type.
    let reader = &*probed;
    Some(SymphoniaReading {
        format_name: reader.format_info().short_name.to_string(),
    })
}

/// Whether Symphonia and this estate agree about what a file is.
///
/// Exposed because it is the comparison this module exists to make. The estate's
/// classification is matched case-insensitively against Symphonia's names, which vary —
/// `mp3`, `iso mp4` — and a name this does not recognise is reported as a disagreement
/// rather than silently accepted, because silently accepting it is how a new container
/// goes unnoticed.
#[must_use]
pub fn agrees(path: &Path, estate: audiobook_core::ContainerKind) -> Option<bool> {
    let reading = read(path)?;
    let lower = reading.format_name.to_ascii_lowercase();
    let matches = match estate {
        audiobook_core::ContainerKind::Mp3Id3 | audiobook_core::ContainerKind::Mp3Bare => {
            lower.contains("mp3")
        }
        audiobook_core::ContainerKind::Mp4 => lower.contains("mp4") || lower.contains("iso"),
        // The estate's container list is `#[non_exhaustive]`, so a new kind is possible.
        // It is reported as a disagreement rather than assumed to match, because silently
        // accepting an unknown container is how a detector drifts without anyone noticing.
        other => {
            let _ = other;
            return Some(false);
        }
    };
    Some(matches)
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
    use std::path::PathBuf;
    use std::process::Command;

    struct TempFile(PathBuf);

    impl TempFile {
        fn bytes(tag: &str, contents: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "conformance-symphonia-{}-{tag}",
                std::process::id()
            ));
            std::fs::write(&path, contents).expect("write");
            TempFile(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_file_that_is_not_audio_is_not_identified() {
        // `None` and "identified as something unexpected" are different facts: one says
        // the file is not audio, the other says this estate's detector disagrees with
        // another implementation about what it is.
        let file = TempFile::bytes("html", b"<html>404 not found</html>");
        assert_eq!(read(file.path()), None);
        assert_eq!(
            agrees(file.path(), audiobook_core::ContainerKind::Mp3Bare),
            None,
            "and a non-file cannot agree or disagree"
        );
    }

    #[test]
    fn an_empty_file_is_not_identified() {
        let file = TempFile::bytes("empty", &[]);
        assert_eq!(read(file.path()), None);
    }

    /// A real MP3 and a real M4B, produced by ffmpeg if it is available.
    ///
    /// Synthetic files are not enough: Symphonia's probe wants actual frame and box
    /// structure, and four kilobytes of a sync word is not that. The fixtures are skipped
    /// rather than passed when ffmpeg is missing, for the same reason as everywhere else
    /// in this harness.
    #[allow(clippy::expect_used, clippy::panic)]
    fn fixtures(tag: &str) -> Option<(TempFile, TempFile)> {
        if !Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "conformance-symphonia-fx-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");

        let mp3 = dir.join("a.mp3");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=5",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "64k",
                mp3.to_str().unwrap(),
            ])
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg could not build the MP3 fixture");

        let m4b = dir.join("b.m4b");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=5",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
                m4b.to_str().unwrap(),
            ])
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg could not build the M4B fixture");

        Some((TempFile(mp3), TempFile(m4b)))
    }

    #[test]
    fn a_real_mp3_identifies_as_mp3_and_agrees_with_the_estate() {
        let Some((mp3, _)) = fixtures("mp3") else {
            eprintln!("SKIPPED: ffmpeg not on PATH, so there is no real fixture to probe");
            return;
        };
        let reading = read(mp3.path()).expect("a real MP3 identifies");
        assert!(
            reading.format_name.to_ascii_lowercase().contains("mp3"),
            "got {:?}",
            reading.format_name
        );
        let bytes = std::fs::read(mp3.path()).expect("read");
        let estate = audiobook_core::MediaProbe::probe(&bytes)
            .container
            .expect("the estate detects it too");
        assert_eq!(
            agrees(mp3.path(), estate),
            Some(true),
            "the estate says {estate:?}, Symphonia says {:?}",
            reading.format_name
        );
    }

    #[test]
    fn a_real_m4b_identifies_as_mp4_and_not_as_mp3() {
        let Some((_, m4b)) = fixtures("m4b") else {
            eprintln!("SKIPPED: ffmpeg not on PATH");
            return;
        };
        let reading = read(m4b.path()).expect("a real M4B identifies");
        assert!(
            reading.format_name.to_ascii_lowercase().contains("mp4"),
            "got {:?}",
            reading.format_name
        );
        assert_ne!(
            agrees(m4b.path(), audiobook_core::ContainerKind::Mp3Id3),
            Some(true),
            "an MP4 must not be classified as an MP3"
        );
    }
}
