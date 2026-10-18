//! Corpus generation: the ffmpeg-driven fixture builder.
//!
//! A process surface (ADR-0005): everything here shells out to ffmpeg and
//! ffprobe. The comparison logic that judges the generated fixtures lives
//! in `lib.rs` and stays under the line gate; this driver is excluded from
//! it, because a line percentage cannot grade code whose substance is
//! driving an external tool. The end-to-end assertions run in CI's
//! quality job, where ffmpeg is installed and the output is checked.

use crate::{Corpus, CorpusFile, Error, FileKind};
use std::path::{Path, PathBuf};
use std::process::Command;


