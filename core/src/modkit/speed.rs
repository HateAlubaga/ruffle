//! Global playback speed.
//!
//! The speed factor scales the time that the host passes to `Player::tick`, so
//! frames, timers and streams all advance faster or slower. `getTimer()` is
//! derived from a [`TimerClock`] anchor that is moved only when the factor
//! changes, so it is continuous and monotonic and never moves `start_time`.

use crate::backend::audio::AudioBackend;
use web_time::Instant;

use crate::FloatDuration;

pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;

/// Scaled clock behind `getTimer()`; copied into `UpdateContext`.
#[derive(Debug, Clone, Copy)]
pub struct TimerClock {
    /// Timer value at `base_instant`, in milliseconds.
    base_ms: f64,
    /// `None` until the speed first leaves normal: plain `now - start_time`.
    base_instant: Option<Instant>,
    factor: f64,
}

impl Default for TimerClock {
    fn default() -> Self {
        Self {
            base_ms: 0.0,
            base_instant: None,
            factor: 1.0,
        }
    }
}

impl TimerClock {
    fn millis_f64(&self, now: Instant, start_time: Instant) -> f64 {
        match self.base_instant {
            Some(base) => {
                self.base_ms + now.duration_since(base).as_secs_f64() * 1000.0 * self.factor
            }
            None => now.duration_since(start_time).as_secs_f64() * 1000.0,
        }
    }

    /// The `getTimer()` value at `now`.
    pub fn millis(&self, now: Instant, start_time: Instant) -> u32 {
        match self.base_instant {
            Some(_) => self.millis_f64(now, start_time) as u64 as u32,
            None => now.duration_since(start_time).as_millis() as u32,
        }
    }

    /// Re-anchors at `now` so the timer stays continuous across the change.
    fn set_factor(&mut self, factor: f64, now: Instant, start_time: Instant) {
        if factor == self.factor {
            return;
        }
        self.base_ms = self.millis_f64(now, start_time);
        self.base_instant = Some(now);
        self.factor = factor;
    }
}

#[derive(Debug)]
pub struct Speed {
    factor: f64,
    clock: TimerClock,
    mute_when_not_normal: bool,
    /// Volume to restore when returning to normal speed, if we muted.
    saved_volume: Option<f32>,
}

impl Default for Speed {
    fn default() -> Self {
        Self {
            factor: 1.0,
            clock: TimerClock::default(),
            mute_when_not_normal: true,
            saved_volume: None,
        }
    }
}

impl Speed {
    pub fn factor(&self) -> f64 {
        self.factor
    }

    pub fn clock(&self) -> TimerClock {
        self.clock
    }

    pub fn is_normal(&self) -> bool {
        self.factor == 1.0
    }

    pub fn mute_when_not_normal(&self) -> bool {
        self.mute_when_not_normal
    }

    /// Sets the speed factor (clamped to `MIN_SPEED..=MAX_SPEED`) and mutes or
    /// restores audio as needed. `start_time` is `Player::start_time`.
    pub fn set(&mut self, factor: f64, audio: &mut dyn AudioBackend, start_time: Instant) {
        let factor = if factor.is_finite() {
            factor.clamp(MIN_SPEED, MAX_SPEED)
        } else {
            1.0
        };
        self.factor = factor;
        self.clock.set_factor(factor, Instant::now(), start_time);
        self.update_mute(audio);
    }

    pub fn set_mute_when_not_normal(&mut self, mute: bool, audio: &mut dyn AudioBackend) {
        self.mute_when_not_normal = mute;
        self.update_mute(audio);
    }

    /// While muted, a volume change from the user only updates the volume
    /// restored later. Returns `true` if the change was taken.
    pub fn intercept_volume(&mut self, volume: f32) -> bool {
        match &mut self.saved_volume {
            Some(saved) => {
                *saved = volume;
                true
            }
            None => false,
        }
    }

    fn update_mute(&mut self, audio: &mut dyn AudioBackend) {
        let should_mute = self.mute_when_not_normal && !self.is_normal();
        match (should_mute, self.saved_volume) {
            (true, None) => {
                self.saved_volume = Some(audio.volume());
                audio.set_volume(0.0);
            }
            (false, Some(volume)) => {
                audio.set_volume(volume);
                self.saved_volume = None;
            }
            _ => {}
        }
    }

    /// Scales a host tick. `getTimer()` follows through [`TimerClock`].
    pub fn scale_tick(&self, dt: FloatDuration) -> FloatDuration {
        if self.is_normal() || dt.as_millis() <= 0.0 {
            return dt;
        }
        FloatDuration::from_millis(dt.as_millis() * self.factor)
    }

    /// Converts a duration of game time into host time.
    pub fn to_host_time(&self, game_time: FloatDuration) -> FloatDuration {
        if self.is_normal() {
            game_time
        } else {
            FloatDuration::from_millis(game_time.as_millis() / self.factor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_time::Duration;

    #[test]
    fn scale_tick_scales_dt() {
        let mut speed = Speed::default();
        let dt = FloatDuration::from_millis(100.0);
        speed.factor = 2.0;
        assert!((speed.scale_tick(dt).as_millis() - 200.0).abs() < 1e-9);
        speed.factor = 0.25;
        assert!((speed.scale_tick(dt).as_millis() - 25.0).abs() < 1e-9);
    }

    #[test]
    fn normal_speed_is_identity() {
        let speed = Speed::default();
        let start = Instant::now();
        let dt = FloatDuration::from_millis(16.0);
        assert_eq!(speed.scale_tick(dt), dt);
        assert_eq!(speed.to_host_time(dt), dt);
        // Never anchored: plain `now - start_time`.
        let now = start + Duration::from_millis(1234);
        assert_eq!(speed.clock().millis(now, start), 1234);
    }

    #[test]
    fn timer_is_monotonic_when_slow() {
        let start = Instant::now();
        let mut clock = TimerClock::default();
        let t0 = start + Duration::from_millis(1000);
        clock.set_factor(0.5, t0, start);
        let mut last = clock.millis(t0, start);
        assert_eq!(last, 1000);
        // Reads between ticks and inside ticks all come from the same clock.
        for i in 1..=200u64 {
            let now = t0 + Duration::from_micros(i * 7_300);
            let value = clock.millis(now, start);
            assert!(value >= last, "{value} < {last}");
            last = value;
        }
        assert_eq!(clock.millis(t0 + Duration::from_millis(1000), start), 1500);
    }

    #[test]
    fn timer_is_continuous_across_factor_change() {
        let start = Instant::now();
        let mut clock = TimerClock::default();
        let t1 = start + Duration::from_millis(2000);
        let before = clock.millis(t1, start);
        clock.set_factor(4.0, t1, start);
        assert_eq!(clock.millis(t1, start), before);

        let t2 = t1 + Duration::from_millis(500);
        let before = clock.millis(t2, start);
        assert_eq!(before, 4000);
        clock.set_factor(0.25, t2, start);
        assert_eq!(clock.millis(t2, start), before);
        clock.set_factor(1.0, t2, start);
        assert_eq!(clock.millis(t2, start), before);
        assert_eq!(clock.millis(t2 + Duration::from_millis(100), start), 4100);
    }

    #[test]
    fn timer_does_not_move_start_time() {
        // Ten hours at x4 is far more than `start_time` could be moved back
        // right after boot; the anchor only ever looks forward.
        let start = Instant::now();
        let mut clock = TimerClock::default();
        clock.set_factor(4.0, start, start);
        let now = start + Duration::from_secs(10 * 3600);
        assert_eq!(clock.millis(now, start), 4 * 10 * 3600 * 1000);
    }
}
