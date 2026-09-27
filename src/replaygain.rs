//! ReplayGain 2.0 track and album gain, as beets' `replaygain` plugin
//! writes it: EBU R128 integrated loudness measured against the -18 LUFS
//! reference, and the sample peak. Players use it to play a quiet 1970s LP
//! and a limited 2020s master at the same loudness.

use std::path::{Path, PathBuf};

use ebur128::{EbuR128, Mode};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// ReplayGain 2.0's reference loudness.
const REFERENCE_LUFS: f64 = -18.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gain {
    pub db: f64,
    /// Largest sample, where 1.0 is full scale.
    pub peak: f64,
}

/// Decode `path` into a loudness meter.
fn measure(path: &Path) -> Result<(EbuR128, f64), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no audio track")?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .cloned()
        .ok_or("no audio parameters")?;
    let track_id = track.id;
    let rate = params.sample_rate.ok_or("unknown sample rate")?;
    let channels = params.channels.as_ref().map_or(2, |c| c.count()) as u32;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let mut meter =
        EbuR128::new(channels, rate, Mode::I | Mode::SAMPLE_PEAK).map_err(|e| e.to_string())?;
    let mut samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(e.to_string()),
        };
        if audio.spec().channels().count() as u32 != channels {
            return Err("channel count changes mid-stream".into());
        }
        audio.copy_to_vec_interleaved(&mut samples);
        meter.add_frames_f32(&samples).map_err(|e| e.to_string())?;
    }
    let peak = (0..channels)
        .filter_map(|c| meter.sample_peak(c).ok())
        .fold(0.0, f64::max);
    Ok((meter, peak))
}

/// Track gains for `paths`, in order, and the album gain across them all.
/// Tracks are measured in parallel, `workers` at a time.
pub fn album(paths: &[PathBuf], workers: usize) -> Result<(Vec<Gain>, Gain), String> {
    let chunk = paths.len().div_ceil(workers.max(1)).max(1);
    let measured: Vec<Result<(EbuR128, f64), String>> = std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| s.spawn(move || part.iter().map(|p| measure(p)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let mut meters = Vec::new();
    let mut tracks = Vec::new();
    for (path, m) in paths.iter().zip(measured) {
        let (meter, peak) = m.map_err(|e| format!("{}: {e}", path.display()))?;
        let loudness = meter.loudness_global().map_err(|e| e.to_string())?;
        tracks.push(Gain {
            db: REFERENCE_LUFS - loudness,
            peak,
        });
        meters.push(meter);
    }
    let loudness = EbuR128::loudness_global_multiple(meters.iter()).map_err(|e| e.to_string())?;
    let album = Gain {
        db: REFERENCE_LUFS - loudness,
        peak: tracks.iter().map(|g| g.peak).fold(0.0, f64::max),
    };
    Ok((tracks, album))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten seconds of a 1 kHz sine at `amplitude`, 16-bit mono.
    fn sine(path: &Path, amplitude: f64) {
        let rate = 44_100u32;
        let samples: Vec<i16> = (0..rate * 10)
            .map(|n| {
                let t = f64::from(n) / f64::from(rate);
                (amplitude * (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * f64::from(i16::MAX))
                    as i16
            })
            .collect();
        let data = (samples.len() * 2) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn a_sine_at_minus_20_dbfs_needs_five_db_and_quieter_tracks_more() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
        sine(&a, 0.1);
        sine(&b, 0.05);
        let (tracks, album) = album(&[a.clone(), b.clone()], 2).unwrap();
        assert!((tracks[0].db - 5.0).abs() < 0.2, "{:?}", tracks[0]);
        assert!((tracks[0].peak - 0.1).abs() < 0.001, "{:?}", tracks[0]);
        assert!(
            (tracks[1].db - tracks[0].db - 6.02).abs() < 0.2,
            "{:?}",
            tracks
        );
        assert!(
            album.db > tracks[0].db && album.db < tracks[1].db,
            "{album:?}"
        );
        assert!((album.peak - 0.1).abs() < 0.001);

        crate::meta::set_replaygain(&a, tracks[0], album).unwrap();
        assert!(crate::meta::has_album_gain(&a));
        assert!(!crate::meta::has_album_gain(&b));
    }
}
