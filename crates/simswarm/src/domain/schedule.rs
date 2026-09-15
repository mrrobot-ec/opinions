//! Tick-granular Poisson-process approximation plus a pinned close spike.

use serde::{Deserialize, Serialize};

use super::rng::CountedRng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub duration_ticks: u64,
    pub close_tick: u64,
    pub close_spike_ticks: u64,
    pub base_rate_ppm: u32,
    pub close_spike_rate_ppm: u32,
}

impl ScheduleConfig {
    #[must_use]
    pub const fn smoke() -> Self {
        Self {
            duration_ticks: 180,
            close_tick: 180,
            // The convoy begins 60s before the 120s hidden offset and
            // continues through close. That makes ticks 60..119 the
            // measurable pre-hidden load window while retaining the final
            // hidden-window worst case.
            close_spike_ticks: 120,
            base_rate_ppm: 20_000,
            close_spike_rate_ppm: 400_000,
        }
    }

    pub const fn validate(self) -> Result<(), ScheduleError> {
        if self.close_tick > self.duration_ticks {
            return Err(ScheduleError::CloseAfterRun);
        }
        if self.close_spike_ticks > self.close_tick {
            return Err(ScheduleError::SpikeTooLong);
        }
        if self.base_rate_ppm > 1_000_000 || self.close_spike_rate_ppm > 1_000_000 {
            return Err(ScheduleError::RateOutOfRange);
        }
        Ok(())
    }

    pub fn opportunity_ticks(self, rng: &mut CountedRng) -> Result<Vec<u64>, ScheduleError> {
        self.validate()?;
        let spike_start = self.close_tick - self.close_spike_ticks;
        let mut ticks = Vec::new();
        for tick in 0..self.duration_ticks {
            let rate = if (spike_start..self.close_tick).contains(&tick) {
                self.close_spike_rate_ppm
            } else {
                self.base_rate_ppm
            };
            if rng.chance_ppm(rate) {
                ticks.push(tick);
            }
        }
        Ok(ticks)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("close tick is after the run")]
    CloseAfterRun,
    #[error("close spike starts before tick zero")]
    SpikeTooLong,
    #[error("rate must be within one million ppm")]
    RateOutOfRange,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::rng::CountedRng;

    #[test]
    fn schedule_consumes_one_draw_per_tick_and_spikes_at_close() {
        let smoke = ScheduleConfig::smoke();
        assert_eq!(smoke.close_tick - smoke.close_spike_ticks, 60);
        let config = ScheduleConfig {
            duration_ticks: 10,
            close_tick: 10,
            close_spike_ticks: 3,
            base_rate_ppm: 0,
            close_spike_rate_ppm: 1_000_000,
        };
        let mut rng = CountedRng::new([5; 32]);
        assert_eq!(config.opportunity_ticks(&mut rng).unwrap(), vec![7, 8, 9]);
        assert_eq!(rng.consumed(), 10);
    }

    #[test]
    fn invalid_schedule_is_rejected() {
        let mut config = ScheduleConfig::smoke();
        config.close_tick = config.duration_ticks + 1;
        assert_eq!(config.validate(), Err(ScheduleError::CloseAfterRun));
        config.close_tick = config.duration_ticks;
        config.close_spike_ticks = config.close_tick + 1;
        assert_eq!(config.validate(), Err(ScheduleError::SpikeTooLong));
        config.close_spike_ticks = 1;
        config.base_rate_ppm = 1_000_001;
        assert_eq!(config.validate(), Err(ScheduleError::RateOutOfRange));
    }
}
