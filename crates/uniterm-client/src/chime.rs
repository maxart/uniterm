//! Audible agent notifications, played by the attached client.
//!
//! The Workspace decides when an agent deserves a sound and what kind (see
//! `Server::deliver_agent_notification`); the client is the only side that
//! knows where the human is, so a remote Workspace chimes on the machine at
//! the keyboard. The `chime` mode synthesises a short two-tone PCM clip in
//! code so it sounds identical everywhere and ships no audio asset, and the
//! `bell` default rides the terminal's own BEL so it works over any transport
//! with the user's existing bell preference. Playback runs on a short-lived
//! detached thread with all stdio silenced, so a missing or hung player can
//! never write into the terminal or stall input.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uniterm_core::NotificationSound;
use uniterm_proto::ChimeKind;

/// Sample rate of the synthesised clip. Low enough to keep the clip a few
/// kilobytes, high enough for tones under 1.5 kHz to be clean.
pub const SAMPLE_RATE: u32 = 22_050;
/// Peak amplitude as a fraction of full scale; a notification, not an alarm.
const PEAK: f32 = 0.35;
/// Two chimes closer than this collapse into one so a fleet finishing at the
/// same moment does not stutter.
const MIN_GAP: Duration = Duration::from_millis(250);
/// A player that has not finished by then is killed; the clip is under a second.
const PLAYER_BOUND: Duration = Duration::from_secs(5);

/// What the caller must still do after [`play`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Playback {
    /// Nothing further; the sound is off or a player was started.
    Handled,
    /// Write BEL to the terminal: either the bell mode was chosen or no audio
    /// player exists on this machine and the bell is the honest fallback.
    Bell,
}

/// Whether this notification should make a sound at all. Attention always
/// does: a waiting permission prompt is the whole point. A completion stays
/// quiet when the human is already looking at that Pane in a focused
/// terminal, because the screen already told them.
pub fn should_sound(kind: ChimeKind, pane_active: bool, terminal_focused: bool) -> bool {
    match kind {
        ChimeKind::Attention => true,
        ChimeKind::Done => !(pane_active && terminal_focused),
    }
}

/// Start the configured sound and say whether the caller owes a BEL.
pub fn play(kind: ChimeKind, sound: NotificationSound, file: &str) -> Playback {
    match sound {
        NotificationSound::Off => Playback::Handled,
        NotificationSound::Bell => Playback::Bell,
        NotificationSound::Chime => {
            if !debounce() {
                return Playback::Handled;
            }
            match spawn_pcm(kind) {
                true => Playback::Handled,
                false => Playback::Bell,
            }
        }
        NotificationSound::File => {
            if !debounce() {
                return Playback::Handled;
            }
            let path = Path::new(file.trim());
            if file.trim().is_empty() || !path.is_file() {
                return Playback::Bell;
            }
            match spawn_file(path) {
                true => Playback::Handled,
                false => Playback::Bell,
            }
        }
    }
}

fn debounce() -> bool {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return true;
    };
    let now = Instant::now();
    if last.is_some_and(|previous| now.duration_since(previous) < MIN_GAP) {
        return false;
    }
    *last = Some(now);
    true
}

/// Synthesise the clip for `kind` as signed 16-bit little-endian mono PCM.
/// Done is a rising major-sixth pair (A5 then E6); attention is two short
/// pulses of the same lower note, so the two are told apart by ear.
pub fn pcm(kind: ChimeKind) -> Vec<i16> {
    let mut samples = Vec::new();
    match kind {
        ChimeKind::Done => {
            tone(&mut samples, 880.0, 130, 0.070);
            tone(&mut samples, 1318.5, 170, 0.090);
        }
        ChimeKind::Attention => {
            tone(&mut samples, 660.0, 90, 0.050);
            silence(&mut samples, 60);
            tone(&mut samples, 660.0, 90, 0.050);
        }
    }
    samples
}

/// Append `millis` of a sine at `hz` with a 5 ms linear attack, an
/// exponential decay of time constant `tau` seconds, and a 10 ms linear
/// release, so it rings rather than buzzes and never clicks at either edge.
fn tone(out: &mut Vec<i16>, hz: f32, millis: u32, tau: f32) {
    let count = SAMPLE_RATE * millis / 1000;
    let attack = SAMPLE_RATE * 5 / 1000;
    let release = SAMPLE_RATE * 10 / 1000;
    let rate = SAMPLE_RATE as f32;
    for index in 0..count {
        let t = index as f32 / rate;
        let decay = if index < attack {
            index as f32 / attack as f32
        } else {
            (-(t - 0.005) / tau).exp()
        };
        let remaining = count - index;
        let envelope = if remaining < release {
            decay * remaining as f32 / release as f32
        } else {
            decay
        };
        let sample = (2.0 * std::f32::consts::PI * hz * t).sin() * envelope * PEAK;
        out.push((sample * f32::from(i16::MAX)) as i16);
    }
}

fn silence(out: &mut Vec<i16>, millis: u32) {
    out.extend(std::iter::repeat_n(
        0,
        (SAMPLE_RATE * millis / 1000) as usize,
    ));
}

fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// A complete RIFF/WAVE file for `samples`: the 44-byte canonical header
/// followed by the PCM. Used where the player wants a file, not a stream.
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data = pcm_bytes(samples);
    let data_len = data.len() as u32;
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// Find `name` on `PATH`; players are optional and looked up per chime so
/// installing one takes effect without a restart.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// A raw-PCM player command reading the clip on stdin, in preference order.
fn pcm_player() -> Option<Command> {
    let rate = SAMPLE_RATE.to_string();
    if let Some(bin) = which("pw-play") {
        let mut cmd = Command::new(bin);
        cmd.args([
            "--raw",
            "--format",
            "s16",
            "--rate",
            &rate,
            "--channels",
            "1",
            "-",
        ]);
        return Some(cmd);
    }
    if let Some(bin) = which("paplay") {
        let mut cmd = Command::new(bin);
        cmd.args(["--raw", "--format=s16le", "--channels=1"]);
        cmd.arg(format!("--rate={rate}"));
        return Some(cmd);
    }
    if let Some(bin) = which("aplay") {
        let mut cmd = Command::new(bin);
        cmd.args(["-q", "-f", "S16_LE", "-c", "1", "-r", &rate]);
        return Some(cmd);
    }
    if let Some(bin) = which("ffplay") {
        let mut cmd = Command::new(bin);
        cmd.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]);
        cmd.args(["-f", "s16le", "-ar", &rate, "-ac", "1", "-"]);
        return Some(cmd);
    }
    None
}

/// A player for an audio file on disk, in preference order.
fn file_player(path: &Path) -> Option<Command> {
    if let Some(bin) = which("afplay") {
        let mut cmd = Command::new(bin);
        cmd.arg(path);
        return Some(cmd);
    }
    for name in ["pw-play", "paplay"] {
        if let Some(bin) = which(name) {
            let mut cmd = Command::new(bin);
            cmd.arg(path);
            return Some(cmd);
        }
    }
    if let Some(bin) = which("ffplay") {
        let mut cmd = Command::new(bin);
        cmd.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]);
        cmd.arg(path);
        return Some(cmd);
    }
    if let Some(bin) = which("mpv") {
        let mut cmd = Command::new(bin);
        cmd.args(["--no-video", "--really-quiet"]);
        cmd.arg(path);
        return Some(cmd);
    }
    None
}

/// Where the synthesised clip is materialised for file-only players (macOS
/// `afplay`). One file per kind per user; rewritten only when missing.
fn clip_path(kind: ChimeKind) -> PathBuf {
    let name = match kind {
        ChimeKind::Done => "done",
        ChimeKind::Attention => "attention",
    };
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("uniterm-chime-{uid}-{name}.wav"))
}

fn ensure_clip(kind: ChimeKind) -> Option<PathBuf> {
    let path = clip_path(kind);
    let bytes = wav(&pcm(kind));
    let fresh = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64);
    if !fresh {
        let tmp = path.with_extension(format!("wav.{}", std::process::id()));
        std::fs::write(&tmp, &bytes).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    Some(path)
}

/// Start the synthesised clip; true when a player was launched.
fn spawn_pcm(kind: ChimeKind) -> bool {
    if let Some(mut cmd) = pcm_player() {
        let bytes = pcm_bytes(&pcm(kind));
        return run_detached(cmd.stdin(Stdio::piped()), Some(bytes));
    }
    // No stream player: fall back to a file player over a materialised WAV.
    match ensure_clip(kind).and_then(|path| file_player(&path)) {
        Some(mut cmd) => run_detached(cmd.stdin(Stdio::null()), None),
        None => false,
    }
}

fn spawn_file(path: &Path) -> bool {
    match file_player(path) {
        Some(mut cmd) => run_detached(cmd.stdin(Stdio::null()), None),
        None => false,
    }
}

/// Spawn the player with stdout and stderr silenced, feed it `input` if any,
/// and reap it from a detached thread bounded by [`PLAYER_BOUND`], so the
/// attach loop never waits on audio and no zombie outlives the chime.
fn run_detached(cmd: &mut Command, input: Option<Vec<u8>>) -> bool {
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    std::thread::Builder::new()
        .name("uniterm-chime".into())
        .spawn(move || {
            if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
                let _ = stdin.write_all(&bytes);
                // Dropping stdin sends EOF so the player can finish.
            }
            let deadline = Instant::now() + PLAYER_BOUND;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_are_short_and_within_the_amplitude_budget() {
        for kind in [ChimeKind::Done, ChimeKind::Attention] {
            let samples = pcm(kind);
            let millis = samples.len() as u32 * 1000 / SAMPLE_RATE;
            assert!((200..=400).contains(&millis), "{kind:?} lasts {millis} ms");
            let ceiling = (PEAK * f32::from(i16::MAX)) as i16 + 1;
            assert!(samples.iter().all(|s| s.abs() <= ceiling), "{kind:?} clips");
            assert!(
                samples.iter().any(|s| s.abs() > ceiling / 2),
                "{kind:?} is silent"
            );
            // Both edges are at rest so the clip cannot click.
            assert_eq!(samples[0], 0);
            assert!(samples.last().is_some_and(|s| s.abs() < 200));
        }
    }

    #[test]
    fn the_two_kinds_sound_different() {
        assert_ne!(pcm(ChimeKind::Done), pcm(ChimeKind::Attention));
    }

    #[test]
    fn wav_has_a_canonical_header_and_consistent_lengths() {
        let samples = pcm(ChimeKind::Done);
        let bytes = wav(&samples);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(&bytes[36..40], b"data");
        let riff_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(riff_len as usize, bytes.len() - 8);
        assert_eq!(data_len as usize, samples.len() * 2);
        assert_eq!(bytes.len(), 44 + samples.len() * 2);
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        assert_eq!(rate, SAMPLE_RATE);
    }

    #[test]
    fn completion_is_quiet_only_for_the_watched_pane_in_a_focused_terminal() {
        assert!(!should_sound(ChimeKind::Done, true, true));
        assert!(should_sound(ChimeKind::Done, true, false));
        assert!(should_sound(ChimeKind::Done, false, true));
        assert!(should_sound(ChimeKind::Attention, true, true));
    }

    #[test]
    fn off_and_bell_never_start_a_player() {
        assert_eq!(
            play(ChimeKind::Done, NotificationSound::Off, ""),
            Playback::Handled
        );
        assert_eq!(
            play(ChimeKind::Done, NotificationSound::Bell, ""),
            Playback::Bell
        );
        // A missing custom file falls back to the bell rather than silence.
        assert_eq!(
            play(
                ChimeKind::Attention,
                NotificationSound::File,
                "/nonexistent/uniterm-chime.wav"
            ),
            Playback::Bell
        );
    }
}
