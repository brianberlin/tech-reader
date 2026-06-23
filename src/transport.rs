//! Transport state for seek and speed (§5.3 / §6.4), shared between the TUI
//! (the only writer) and the synth worker, feeder, and audio callback (readers).
//!
//! A single monotonic `generation` counter is the seek/speed signal: the TUI
//! bumps it (after publishing the new target/speed) on every prev/next/jump or
//! speed step. The readers each watch it and react when it changes —
//!  - the **callback** drains the stale ring and resets the consumed counter,
//!  - the **feeder** rebuilds the boundary table and resets the pushed counter,
//!  - the **synth worker** reseats its sentence cursor (and clears the PCM cache
//!    when the speed changed).
//!
//! All ops use `SeqCst` so a reader that observes a new `generation` is
//! guaranteed to also see the `seek_target`/`speed` written before it. Seeks are
//! rare, so the ordering cost is irrelevant.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst};

pub struct Transport {
    generation: AtomicU64,
    seek_target: AtomicUsize,
    /// `f32::to_bits` of the pitch-preserving speed multiplier (1.0 = normal).
    speed_bits: AtomicU32,
}

impl Transport {
    pub fn new(speed: f32) -> Self {
        Self {
            generation: AtomicU64::new(0),
            seek_target: AtomicUsize::new(0),
            speed_bits: AtomicU32::new(speed.to_bits()),
        }
    }

    /// The current generation; bumped on every seek/speed change.
    pub fn generation(&self) -> u64 {
        self.generation.load(SeqCst)
    }

    /// The sentence index the synth should (re)start from after the latest change.
    pub fn seek_target(&self) -> usize {
        self.seek_target.load(SeqCst)
    }

    /// The current pitch-preserving speed multiplier.
    pub fn speed(&self) -> f32 {
        f32::from_bits(self.speed_bits.load(SeqCst))
    }

    /// TUI: jump playback to `target` (speed unchanged).
    pub fn seek_to(&self, target: usize) {
        self.seek_target.store(target, SeqCst);
        self.generation.fetch_add(1, SeqCst);
    }

    /// TUI: change speed, resuming from `current` so it takes effect at the
    /// audible position rather than wherever synthesis happens to be.
    pub fn set_speed(&self, speed: f32, current: usize) {
        self.speed_bits.store(speed.to_bits(), SeqCst);
        self.seek_target.store(current, SeqCst);
        self.generation.fetch_add(1, SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_bumps_generation_and_publishes_target() {
        let t = Transport::new(1.0);
        assert_eq!(t.generation(), 0);
        assert_eq!(t.seek_target(), 0);
        t.seek_to(7);
        assert_eq!(t.generation(), 1);
        assert_eq!(t.seek_target(), 7);
        assert_eq!(t.speed(), 1.0); // unchanged by a seek
    }

    #[test]
    fn set_speed_publishes_speed_and_resumes_at_current() {
        let t = Transport::new(1.0);
        t.set_speed(1.5, 4);
        assert_eq!(t.generation(), 1);
        assert_eq!(t.speed(), 1.5);
        assert_eq!(t.seek_target(), 4);
    }
}
