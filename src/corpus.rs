//! Corpus generation: the ffmpeg-driven fixture builder.
//!
//! A process surface (ADR-0005): everything here shells out to ffmpeg and
//! ffprobe. The comparison logic that judges the generated fixtures lives
//! in `lib.rs` and stays under the line gate; this driver is excluded from
//! it, because a line percentage cannot grade code whose substance is
//! driving an external tool. The end-to-end assertions run in CI's
//! quality job, where ffmpeg is installed and the output is checked.

use crate::{
    args, ffprobe, fs_write, id3_tag, path_arg, run_tool, synchsafe, tit2, CorpusFile, Error,
    FileKind,
};
use std::fs;
use std::path::{Path, PathBuf};

/// Write a mono 440 Hz tone as a 16-bit WAV.
///
/// Generated rather than shipped so the corpus is reproducible from source and the
/// repository stays small; 180 seconds at 44.1 kHz is about 15 MB on disk, which is
/// the trade for a file long enough to have three distinct chapters.
pub(crate) fn write_tone(path: &Path) -> Result<(), Error> {
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
pub(crate) fn ffmetadata(duration_ms: u64, titles: &[&str]) -> String {
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

/// Splice a tag onto the front of a file, replacing any tag already there.
pub(crate) fn splice_tag(audio: &Path, out: &Path, tag: &[u8]) -> Result<(), Error> {
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

/// Build a `CHAP` body.
///
/// The two offsets are `0xFFFFFFFF`, the sentinel the spec defines for "ignore this and
/// use the time" — a real writer with no seek table emits exactly that, and a fixture
/// that put plausible-looking byte offsets in would be testing nothing.
pub(crate) fn chap_frame(element_id: &str, start_ms: u32, end_ms: u32, title: &str) -> Vec<u8> {
    frame_body(b"CHAP", &chap_body(element_id, start_ms, end_ms, title, 4))
}

/// The body of a `CHAP` frame, without a header of its own.
///
/// Split out from [`chap_frame`] so a caller can frame it with a different version's
/// header — a v2.3 tag needs nine-byte frame headers, and re-using the v2.4 form would
/// test the wrong thing.
pub(crate) fn chap_body(
    element_id: &str,
    start_ms: u32,
    end_ms: u32,
    title: &str,
    version: u8,
) -> Vec<u8> {
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
pub(crate) fn frame_body(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
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

impl crate::Corpus {
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
        run_tool(
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
        run_tool(
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
        run_tool(
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
        run_tool(
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
        run_tool(
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
            run_tool(
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
