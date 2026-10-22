//! What the status LED shows.
//!
//! The device lives in a ceiling or a cabinet, so the LED is the only feedback
//! available without a serial cable. Each state gets a pattern distinct enough
//! to recognise from across the room, and the logic is pure (a stage and a
//! timestamp in, on/off out) so the main loop stays a single call.

use crate::pairing::Stage;

/// The states worth looking at from the outside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pattern {
    /// Idle: a short pulse every two seconds.
    Heartbeat,
    /// Pairing is listening for the four-button sequence.
    Pairing,
    /// A remote was just learned (shown while the stage reports `Paired`).
    Learned,
    /// Pairing timed out or had nowhere to store the remote.
    Failed,
    /// The coordinator asked us to identify ourselves.
    Identify,
}

/// Priority: identify wins over everything (a human asked for it), then the
/// transient pairing result, then pairing itself, then the idle heartbeat.
pub fn pattern(stage: Stage, pairing: bool, identifying: bool) -> Pattern {
    if identifying {
        return Pattern::Identify;
    }
    match stage {
        Stage::Paired => Pattern::Learned,
        Stage::Failed => Pattern::Failed,
        _ if pairing => Pattern::Pairing,
        _ => Pattern::Heartbeat,
    }
}

/// LED level at `now_ms` for the given pattern.
pub fn is_on(pattern: Pattern, now_ms: u64) -> bool {
    match pattern {
        // brief pulse every 2 s: on, not blinking
        Pattern::Heartbeat => now_ms % 2_000 < 80,
        // 4 Hz while waiting for the remote
        Pattern::Pairing => now_ms % 250 < 125,
        // solid while the "paired" result is on show
        Pattern::Learned => true,
        // recognisable double blink: short, gap, short, long pause
        Pattern::Failed => {
            let phase = now_ms % 1_400;
            phase < 140 || (300..440).contains(&phase)
        }
        // fast blink while the coordinator identifies the device
        Pattern::Identify => now_ms % 160 < 80,
    }
}
