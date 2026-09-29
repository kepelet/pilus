//! Notification cues synthesized from a server `SoundSignal`.
//!
//! The wire carries only the cue (`Done`/`Blocked`) and its style, never
//! audio, so the window owns the waveform. We render a short mono 16-bit WAV
//! in memory and hand it to the host player on a worker thread, which keeps
//! `afplay` off the GUI thread and lets the cue outlive the poll that queued
//! it.

use l3s_protocol::{SoundCue, SoundSignal, SoundStyle};

/// Sample rate for every generated cue.
const SAMPLE_RATE: u32 = 44_100;

/// Queue `signal` for playback without blocking the caller.
pub fn play(signal: SoundSignal) {
    let audio = wav(&samples(signal));

    // `afplay` is the one audio sink present on every supported macOS install;
    // other targets keep the cue silent rather than pulling in a backend.
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || play_wav(audio));

    #[cfg(not(target_os = "macos"))]
    let _ = audio;
}

/// Write `audio` to a scratch file and play it, then clean up.
#[cfg(target_os = "macos")]
fn play_wav(audio: Vec<u8>) {
    let path = std::env::temp_dir().join(scratch_name());
    if std::fs::write(&path, audio).is_err() {
        return;
    }
    let _ = std::process::Command::new("afplay").arg(&path).status();
    let _ = std::fs::remove_file(&path);
}

/// A collision-resistant scratch name: two windows may fire the same cue at
/// once, so the pid alone is not enough.
#[cfg(target_os = "macos")]
fn scratch_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("l3s-cue-{}-{n}.wav", std::process::id())
}

/// The cue's PCM samples: a short note run shaped by the style, with a linear
/// attack/release so each note starts and stops without a click.
fn samples(signal: SoundSignal) -> Vec<i16> {
    let notes: &[f32] = match signal.cue {
        SoundCue::Done => &[659.25, 783.99, 1046.50],
        SoundCue::Blocked => &[880.00, 587.33],
    };
    let note_ms = match signal.style {
        SoundStyle::Pulse => 70,
        SoundStyle::Retro => 110,
        SoundStyle::Soft => 150,
    };
    let per_note = SAMPLE_RATE * note_ms / 1000;
    let mut out = Vec::with_capacity(notes.len() * per_note as usize);
    for frequency in notes {
        for index in 0..per_note {
            let phase = (index as f32 * frequency / SAMPLE_RATE as f32).fract();
            let wave = match signal.style {
                // Duty-cycled square waves: 50% reads as retro, 25% as pulse.
                SoundStyle::Retro => {
                    if phase < 0.5 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                SoundStyle::Pulse => {
                    if phase < 0.25 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                SoundStyle::Soft => 4.0 * (phase - 0.5).abs() - 1.0,
            };
            let envelope = envelope(index, per_note);
            out.push((wave * envelope * f32::from(i16::MAX) * 0.22) as i16);
        }
    }
    out
}

/// Linear fade over the first and last 6 ms of a note.
fn envelope(index: u32, length: u32) -> f32 {
    let ramp = (SAMPLE_RATE * 6 / 1000).max(1);
    let attack = (index as f32 / ramp as f32).min(1.0);
    let release = ((length.saturating_sub(index)) as f32 / ramp as f32).min(1.0);
    attack.min(release)
}

/// Wrap PCM samples in a canonical 44-byte WAV header.
fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // linear PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_the_payload() {
        let bytes = wav(&[0, 1, -1]);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])), 42);
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap_or([0; 4])), 6);
    }

    #[test]
    fn cue_length_follows_style_and_cue() {
        let pulse = samples(SoundSignal {
            cue: SoundCue::Done,
            style: SoundStyle::Pulse,
        });
        let soft = samples(SoundSignal {
            cue: SoundCue::Done,
            style: SoundStyle::Soft,
        });
        // Done is three notes; a soft note is longer than a pulse note.
        assert_eq!(pulse.len(), 3 * (SAMPLE_RATE * 70 / 1000) as usize);
        assert!(soft.len() > pulse.len());
        assert!(pulse.iter().any(|sample| *sample != 0));
    }
}
