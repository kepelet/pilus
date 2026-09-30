//! Notification cues synthesized from a server `SoundSignal`.
//!
//! The wire carries only the cue (`Done`/`Blocked`) and its style, never
//! audio, so the window owns the waveform. This mirrors the daemon's own
//! synthesizer sample for sample, which keeps a cue identical whether it is
//! played by the TUI or by a thin client.

use pilus_protocol::{SoundCue, SoundSignal, SoundStyle};

/// Sample rate for every generated cue; the daemon uses the same.
const SAMPLE_RATE: u32 = 22_050;

/// Queue `signal` for playback without blocking the caller.
pub fn play(signal: SoundSignal) {
    let audio = synth_wav(signal);

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
    // Absolute path: a bundled app inherits Finder's bare PATH, and `afplay`
    // lives in `/usr/bin` on every macOS install.
    let _ = std::process::Command::new("/usr/bin/afplay")
        .arg(&path)
        .status();
    let _ = std::fs::remove_file(&path);
}

/// A collision-resistant scratch name: two windows may fire the same cue at
/// once, so the pid alone is not enough.
#[cfg(target_os = "macos")]
fn scratch_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("pilus-cue-{}-{n}.wav", std::process::id())
}

/// Which oscillator a cue's notes are drawn with.
#[derive(Clone, Copy)]
enum Waveform {
    Square,
    Triangle,
    Pulse,
}

/// Synthesize one complete notification cue as 16-bit mono PCM WAV.
///
/// Keep the note table aligned with the daemon's `sound::synth_wav`; a cue
/// that sounds different between the two clients reads as a bug.
fn synth_wav(signal: SoundSignal) -> Vec<u8> {
    let mut samples = Vec::new();
    match (signal.style, signal.cue) {
        (SoundStyle::Retro, SoundCue::Done) => {
            for (index, frequency) in [523.25, 659.25, 783.99, 1046.5].into_iter().enumerate() {
                let duration = if index == 3 { 180 } else { 90 };
                push_tone(&mut samples, frequency, duration, 0.22, Waveform::Square);
            }
        }
        (SoundStyle::Retro, SoundCue::Blocked) => {
            push_tone(&mut samples, 392.0, 120, 0.20, Waveform::Square);
            push_silence(&mut samples, 55);
            push_tone(&mut samples, 293.66, 180, 0.20, Waveform::Square);
        }
        (SoundStyle::Soft, SoundCue::Done) => {
            push_tone(&mut samples, 523.25, 150, 0.14, Waveform::Triangle);
            push_tone(&mut samples, 783.99, 230, 0.14, Waveform::Triangle);
        }
        (SoundStyle::Soft, SoundCue::Blocked) => {
            push_tone(&mut samples, 440.0, 125, 0.13, Waveform::Triangle);
            push_silence(&mut samples, 90);
            push_tone(&mut samples, 440.0, 125, 0.13, Waveform::Triangle);
        }
        (SoundStyle::Pulse, SoundCue::Done) => {
            push_tone(&mut samples, 659.25, 70, 0.19, Waveform::Pulse);
            push_silence(&mut samples, 35);
            push_tone(&mut samples, 987.77, 120, 0.19, Waveform::Pulse);
        }
        (SoundStyle::Pulse, SoundCue::Blocked) => {
            push_tone(&mut samples, 329.63, 85, 0.20, Waveform::Pulse);
            push_silence(&mut samples, 55);
            push_tone(&mut samples, 329.63, 85, 0.20, Waveform::Pulse);
            push_silence(&mut samples, 55);
            push_tone(&mut samples, 329.63, 120, 0.20, Waveform::Pulse);
        }
    }
    wav_bytes(&samples)
}

/// Append one faded tone to `samples`.
fn push_tone(
    samples: &mut Vec<i16>,
    frequency: f32,
    duration_ms: u32,
    amplitude: f32,
    waveform: Waveform,
) {
    let count = SAMPLE_RATE * duration_ms / 1000;
    // A ~5 ms fade in and out keeps each note from clicking at its edges.
    let fade = (SAMPLE_RATE / 200).max(1);
    for index in 0..count {
        let phase = (index as f32 * frequency / SAMPLE_RATE as f32) % 1.0;
        let wave = match waveform {
            Waveform::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
            Waveform::Pulse => {
                if phase < 0.22 {
                    1.0
                } else {
                    -0.35
                }
            }
        };
        let fade_in = index.min(fade) as f32 / fade as f32;
        let fade_out = count.saturating_sub(index).min(fade) as f32 / fade as f32;
        samples.push((wave * amplitude * fade_in.min(fade_out) * f32::from(i16::MAX)) as i16);
    }
}

/// Append `duration_ms` of silence.
fn push_silence(samples: &mut Vec<i16>, duration_ms: u32) {
    samples.resize(
        samples.len() + (SAMPLE_RATE * duration_ms / 1000) as usize,
        0,
    );
}

/// Wrap PCM samples in a canonical 44-byte WAV header.
fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_has_distinct_valid_done_and_blocked_wavs() {
        for style in [SoundStyle::Retro, SoundStyle::Soft, SoundStyle::Pulse] {
            let done = synth_wav(SoundSignal {
                cue: SoundCue::Done,
                style,
            });
            let blocked = synth_wav(SoundSignal {
                cue: SoundCue::Blocked,
                style,
            });
            for wav in [&done, &blocked] {
                assert_eq!(&wav[0..4], b"RIFF");
                assert_eq!(&wav[8..12], b"WAVE");
                assert_eq!(&wav[12..16], b"fmt ");
                assert_eq!(&wav[36..40], b"data");
                let data_len =
                    u32::from_le_bytes(wav[40..44].try_into().unwrap_or([0; 4])) as usize;
                assert_eq!(wav.len(), 44 + data_len);
                assert!(data_len > 0);
            }
            assert_ne!(done, blocked, "cues must be recognizable");
        }
    }
}
