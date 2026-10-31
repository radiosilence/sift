//! Whether a file's audio decodes cleanly, as beets' `badfiles` plugin
//! asks with `flac -t` and `mp3val`: a truncated download or a corrupt
//! packet otherwise passes import, since tags and duration read fine, and is
//! found only when it plays. Decoded here with symphonia, so no external
//! tool is needed, and FLAC's stored MD5 of the audio is checked as well.

use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Ok,
    /// The audio is damaged, and how.
    Bad(String),
    /// This format cannot be decoded here, so nothing is claimed about it.
    Unchecked(String),
}

/// Decode every packet of `path`.
pub fn check(path: &Path) -> Verdict {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Verdict::Bad(format!("cannot open: {e}")),
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = match symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(f) => f,
        Err(Error::Unsupported(what)) => return Verdict::Unchecked(what.to_string()),
        Err(e) => return Verdict::Bad(format!("unreadable container: {e}")),
    };
    let Some(track) = format.default_track(TrackType::Audio) else {
        return Verdict::Bad("no audio track".into());
    };
    let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()).cloned() else {
        return Verdict::Bad("no audio parameters".into());
    };
    let (track_id, expected) = (track.id, track.num_frames);
    let mut options = AudioDecoderOptions::default();
    options.verify = true;
    let mut decoder = match symphonia::default::get_codecs().make_audio_decoder(&params, &options) {
        Ok(d) => d,
        Err(Error::Unsupported(what)) => return Verdict::Unchecked(what.to_string()),
        Err(e) => return Verdict::Bad(format!("no decoder: {e}")),
    };
    let (mut frames, mut damaged) = (0u64, 0usize);
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Verdict::Bad(format!("unreadable at frame {frames}: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio) => frames += audio.frames() as u64,
            Err(Error::DecodeError(_)) => damaged += 1,
            Err(e) => return Verdict::Bad(format!("decode failed at frame {frames}: {e}")),
        }
    }
    if damaged > 0 {
        return Verdict::Bad(format!("{damaged} damaged packets"));
    }
    // Lossy codecs pad and trim; a shortfall beyond a second is truncation.
    if let (Some(expected), Some(rate)) = (expected, params.sample_rate)
        && expected > frames + u64::from(rate)
    {
        return Verdict::Bad(format!(
            "truncated: {:.1}s of {:.1}s",
            frames as f64 / f64::from(rate),
            expected as f64 / f64::from(rate)
        ));
    }
    if decoder.finalize().verify_ok == Some(false) {
        return Verdict::Bad("audio does not match its stored MD5".into());
    }
    Verdict::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_file_passes_and_a_cut_short_one_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.wav");
        crate::library::tests::wav(&whole, 44_100 * 4);
        assert_eq!(check(&whole), Verdict::Ok);

        let cut = dir.path().join("cut.wav");
        let bytes = std::fs::read(&whole).unwrap();
        std::fs::write(&cut, &bytes[..bytes.len() / 3]).unwrap();
        assert!(
            matches!(check(&cut), Verdict::Bad(ref why) if why.starts_with("truncated")),
            "{:?}",
            check(&cut)
        );
    }

    #[test]
    fn verdicts_are_kept_until_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.wav");
        crate::library::tests::wav(&f, 44_100);
        let mut lib = crate::library::Library::in_memory().unwrap();
        lib.update(dir.path()).unwrap();
        let items = lib.items(&Default::default()).unwrap();
        assert_eq!(lib.check(&items).unwrap()[0].1, Verdict::Ok);
        std::fs::write(&f, b"RIFF").unwrap();
        lib.update(dir.path()).unwrap();
        let items = lib.items(&Default::default()).unwrap();
        assert!(
            items.is_empty(),
            "a file that no longer reads leaves the index"
        );
    }
}
