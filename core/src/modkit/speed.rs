//! Global playback speed.
//!
//! The speed factor scales the time that the host passes to `Player::tick`, so
//! frames, timers and streams all advance faster or slower. `getTimer()` is
//! computed as `now - start_time`, so `start_time` is shifted on every tick by
//! the difference between scaled and real time. That keeps `getTimer()`
//! consistent with the scaled frame clock without touching the AVM code.

use crate::backend::audio::AudioBackend;
use web_time::{Duration, Instant};

use crate::FloatDuration;

pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;

#[derive(Debug)]
pub struct Speed {
    factor: f64,
    mute_when_not_normal: bool,
    /// Volume to restore when returning to normal speed, if we muted.
    saved_volume: Option<f32>,
}

impl Default for Speed {
    fn default() -> Self {
        Self {
            factor: 1.0,
            mute_when_not_normal: true,
            saved_volume: None,
        }
    }
}

impl Speed {
    pub fn factor(&self) -> f64 {
        self.factor
    }

    pub fn is_normal(&self) -> bool {
        self.factor == 1.0
    }

    pub fn mute_when_not_normal(&self) -> bool {
        self.mute_when_not_normal
    }

    /// Sets the speed factor (clamped to `MIN_SPEED..=MAX_SPEED`) and mutes or
    /// restores audio as needed.
    pub fn set(&mut self, factor: f64, audio: &mut dyn AudioBackend) {
        let factor = if factor.is_finite() {
            factor.clamp(MIN_SPEED, MAX_SPEED)
        } else {
            1.0
        };
        self.factor = factor;
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

    /// Scales a host tick and shifts `start_time` so `getTimer()` follows.
    pub fn scale_tick(&self, dt: FloatDuration, start_time: &mut Instant) -> FloatDuration {
        if self.is_normal() || dt.as_millis() <= 0.0 {
            return dt;
        }
        let scaled = FloatDuration::from_millis(dt.as_millis() * self.factor);
        let shift_millis = scaled.as_millis() - dt.as_millis();
        let shift = Duration::from_secs_f64(shift_millis.abs() / 1000.0);
        let shifted = if shift_millis > 0.0 {
            // Faster: pretend we started earlier.
            start_time.checked_sub(shift)
        } else {
            // Slower: pretend we started later.
            start_time.checked_add(shift)
        };
        if let Some(shifted) = shifted {
            *start_time = shifted;
        }
        scaled
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

    fn assert_close(actual: Duration, expected: Duration) {
        assert!(
            actual.abs_diff(expected) < Duration::from_micros(1),
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn scale_tick_shifts_start_time() {
        let mut speed = Speed::default();
        speed.factor = 2.0;
        let origin = Instant::now();
        let mut start = origin;
        let dt = FloatDuration::from_millis(100.0);
        let scaled = speed.scale_tick(dt, &mut start);
        assert!((scaled.as_millis() - 200.0).abs() < 1e-9);
        // getTimer() gains an extra 100ms on top of the real 100ms.
        assert_close(origin - start, Duration::from_millis(100));

        speed.factor = 0.25;
        let mut start = origin;
        let scaled = speed.scale_tick(dt, &mut start);
        assert!((scaled.as_millis() - 25.0).abs() < 1e-9);
        assert_close(start - origin, Duration::from_millis(75));
    }

    #[test]
    fn normal_speed_is_identity() {
        let speed = Speed::default();
        let origin = Instant::now();
        let mut start = origin;
        let dt = FloatDuration::from_millis(16.0);
        assert_eq!(speed.scale_tick(dt, &mut start), dt);
        assert_eq!(start, origin);
        assert_eq!(speed.to_host_time(dt), dt);
    }
}
