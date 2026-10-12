//! Correlate rendered source frames with Windows' monotonic presentation clocks.
//! These measurements cannot see latency inside an external speaker or display.
use crate::timeline::ReadMapping;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub source_frame: f64,
    pub qpc_hns: u64,
    pub source_rate: f64,
    pub device_rate_ratio: f64,
    pub applied_delay_ms: f64,
}

struct Segment {
    start: u64,
    end: u64,
    mapping: ReadMapping,
}

pub struct PlaybackTracker {
    segments: VecDeque<Segment>,
    written: u64,
    processing_delay: u64,
    nominal_rate: f64,
    last_clock: Option<(f64, u64)>,
    last_seen: Option<(f64, u64)>,
    ratio: f64,
}

impl PlaybackTracker {
    pub fn new(rate: u32, silent_prefill: usize) -> Self {
        Self {
            segments: VecDeque::new(),
            written: silent_prefill as u64,
            processing_delay: 0,
            nominal_rate: rate as f64,
            last_clock: None,
            last_seen: None,
            ratio: 1.0,
        }
    }
    pub fn with_processing_delay(rate: u32, silent_prefill: usize, delay: usize) -> Self {
        let mut tracker = Self::new(rate, silent_prefill);
        tracker.processing_delay = delay as u64;
        tracker
    }
    pub fn record(&mut self, frames: usize, mapping: ReadMapping) {
        self.segments.push_back(Segment {
            start: self.written + self.processing_delay,
            end: self.written + frames as u64 + self.processing_delay,
            mapping,
        });
        self.written += frames as u64;
        // Bound memory even if a defective clock never advances.
        while self.segments.len() > 1024 {
            self.segments.pop_front();
        }
    }
    pub fn observe(&mut self, ticks: u64, frequency: u64, qpc_hns: u64) -> Option<Observation> {
        if frequency == 0 || qpc_hns == 0 {
            return None;
        }
        let played = ticks as f64 / frequency as f64 * self.nominal_rate;
        if played > self.written as f64 + 2.0 {
            return None;
        }
        if self
            .last_seen
            .is_some_and(|(frame, qpc)| played < frame || qpc_hns <= qpc)
        {
            return None;
        }
        self.last_seen = Some((played, qpc_hns));
        if let Some((old_frame, old_qpc)) = self.last_clock {
            if qpc_hns <= old_qpc || played < old_frame {
                return None;
            }
            if qpc_hns - old_qpc >= 2_500_000 {
                let ratio = (played - old_frame)
                    / self.nominal_rate
                    / ((qpc_hns - old_qpc) as f64 / 10_000_000.0);
                if (0.99..=1.01).contains(&ratio) {
                    self.ratio += 0.1 * (ratio - self.ratio);
                }
                self.last_clock = Some((played, qpc_hns));
            }
        } else {
            self.last_clock = Some((played, qpc_hns));
        }
        while self
            .segments
            .front()
            .is_some_and(|s| s.end as f64 <= played)
        {
            self.segments.pop_front();
        }
        let segment = self.segments.front()?;
        if played < segment.start as f64 || !segment.mapping.valid {
            return None;
        }
        Some(Observation {
            source_frame: segment.mapping.source_start
                + (played - segment.start as f64) * segment.mapping.step,
            qpc_hns,
            source_rate: self.nominal_rate * segment.mapping.step * self.ratio,
            device_rate_ratio: self.ratio,
            applied_delay_ms: segment.mapping.delay_ms,
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Plan {
    pub auto_delay_ms: f64,
    pub reference_step: Option<f64>,
    pub error_ms: Option<f64>,
    pub measuring: bool,
}

#[derive(Default)]
struct Member {
    observation: Option<Observation>,
    estimate: Option<f64>,
    auto_delay_ms: f64,
    error_ms: Option<f64>,
    samples: u32,
    last_change: u64,
}

pub struct Alignment {
    pub main: u64,
    automatic: bool,
    rate: f64,
    members: BTreeMap<u64, Member>,
}

impl Alignment {
    pub fn new(rate: u32) -> Self {
        Self {
            main: 0,
            automatic: true,
            rate: rate as f64,
            members: BTreeMap::new(),
        }
    }
    pub fn register(&mut self, id: u64) {
        self.members.entry(id).or_default();
    }
    pub fn remove(&mut self, id: u64) {
        self.members.remove(&id);
        if self.main == id {
            self.main = 0;
        }
        if self.members.len() <= 1 {
            for member in self.members.values_mut() {
                member.auto_delay_ms = 0.0;
                member.estimate = None;
                member.samples = 0;
            }
        }
    }
    pub fn select_clock(&mut self, main: u64) {
        self.main = main;
    }
    pub fn set_automatic(&mut self, automatic: bool) {
        if self.automatic != automatic {
            self.automatic = automatic;
            self.realign();
        }
    }
    pub fn realign(&mut self) {
        for member in self.members.values_mut() {
            member.estimate = None;
            member.samples = 0;
            member.last_change = 0;
            if !self.automatic {
                member.auto_delay_ms = 0.0;
            }
        }
    }
    fn fresh(observation: Observation, now: u64) -> bool {
        // Workers sample at slightly different times; permit 50 ms lookahead.
        observation.qpc_hns <= now.saturating_add(500_000)
            && now.saturating_sub(observation.qpc_hns) < 3_000_000
    }
    fn position(observation: Observation, now: u64) -> f64 {
        observation.source_frame
            + (now as f64 - observation.qpc_hns as f64) / 10_000_000.0 * observation.source_rate
    }
    pub fn observe(&mut self, id: u64, observation: Option<Observation>, now: u64) {
        let Some(member) = self.members.get_mut(&id) else {
            return;
        };
        member.observation = observation.filter(|o| {
            o.source_frame.is_finite()
                && o.source_rate.is_finite()
                && (self.rate * 0.98..=self.rate * 1.02).contains(&o.source_rate)
                && o.applied_delay_ms.is_finite()
                && (0.0..=1000.0).contains(&o.applied_delay_ms)
                && Self::fresh(*o, now)
        });
        let candidates: Vec<_> = self
            .members
            .iter()
            .filter_map(|(&id, member)| {
                let o = member.observation.filter(|o| Self::fresh(*o, now))?;
                // Undo the delay ACTUALLY rendered in this segment, not the newest
                // command. Otherwise automatic alignment oscillates or cancels trim.
                let baseline = Self::position(o, now) + o.applied_delay_ms * self.rate / 1000.0;
                Some((id, baseline, Self::position(o, now)))
            })
            .collect();
        if candidates.len() < 2 {
            for member in self.members.values_mut() {
                member.error_ms = None;
            }
            return;
        }
        let slowest = candidates.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
        for (id, baseline, played) in candidates {
            let member = self.members.get_mut(&id).unwrap();
            member.error_ms = Some((played - slowest) * 1000.0 / self.rate);
            if !self.automatic {
                continue;
            }
            let wanted = ((baseline - slowest) * 1000.0 / self.rate).clamp(0.0, 500.0);
            member.estimate = Some(
                member
                    .estimate
                    .map_or(wanted, |old| old + (wanted - old) * 0.04),
            );
            member.samples = member.samples.saturating_add(1);
            if member.samples >= 24 && now.saturating_sub(member.last_change) >= 5_000_000 {
                let estimate = member.estimate.unwrap();
                if (member.auto_delay_ms - estimate).abs() > 0.5 {
                    member.auto_delay_ms = estimate;
                    member.last_change = now;
                }
            }
        }
    }
    pub fn plan(&self, id: u64, now: u64) -> Plan {
        let Some(member) = self.members.get(&id) else {
            return Plan::default();
        };
        let own = member.observation.filter(|o| Self::fresh(*o, now));
        let reference_step = if self.main != 0 && self.main != id {
            let main = self
                .members
                .get(&self.main)
                .and_then(|m| m.observation)
                .filter(|o| Self::fresh(*o, now));
            main.zip(own)
                .map(|(main, own)| main.source_rate / (self.rate * own.device_rate_ratio))
        } else {
            None
        };
        Plan {
            auto_delay_ms: if self.automatic {
                member.auto_delay_ms
            } else {
                0.0
            },
            reference_step,
            error_ms: own.and(member.error_ms),
            measuring: self.automatic
                && (own.is_none()
                    || (self.members.len() > 1
                        && (member.samples < 24
                            || self
                                .members
                                .values()
                                .filter(|m| m.observation.is_some_and(|o| Self::fresh(o, now)))
                                .count()
                                < 2))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation(frame: f64, time: u64, delay: f64) -> Observation {
        Observation {
            source_frame: frame,
            qpc_hns: time,
            source_rate: 48000.0,
            device_rate_ratio: 1.0,
            applied_delay_ms: delay,
        }
    }
    #[test]
    fn automatic_delay_survives_alignment_and_does_not_cancel_manual_trim() {
        let mut align = Alignment::new(48000);
        align.register(1);
        align.register(2);
        for i in 0..80 {
            let now = 10_000_000 + i * 100_000;
            let frame = i as f64 * 480.0;
            align.observe(1, Some(observation(frame + 960.0, now, 0.0)), now);
            align.observe(2, Some(observation(frame, now, 0.0)), now);
        }
        assert!((align.plan(1, 18_000_000).auto_delay_ms - 20.0).abs() < 0.01);
        // Now device 1 has the automatic 20 ms plus a manual 10 ms delay.
        for i in 80..180 {
            let now = 10_000_000 + i * 100_000;
            let frame = i as f64 * 480.0;
            align.observe(1, Some(observation(frame - 480.0, now, 30.0)), now);
            align.observe(2, Some(observation(frame, now, 0.0)), now);
        }
        let plan = align.plan(1, 27_900_000);
        assert!((plan.auto_delay_ms - 20.0).abs() < 0.01);
        assert!((plan.error_ms.unwrap() + 10.0).abs() < 0.01);
        align.set_automatic(false);
        assert_eq!(align.plan(1, 27_900_000).auto_delay_ms, 0.0);
    }
    #[test]
    fn correlates_different_qpc_timestamps_and_rejects_stale_members() {
        let mut align = Alignment::new(48000);
        align.register(1);
        align.register(2);
        align.observe(1, Some(observation(480.0, 10_000_000, 0.0)), 10_000_000);
        align.observe(2, Some(observation(960.0, 10_100_000, 0.0)), 10_100_000);
        assert!(align.plan(1, 10_100_000).error_ms.unwrap().abs() < 0.001);
        align.select_clock(1);
        assert_eq!(align.plan(2, 10_100_000).reference_step, Some(1.0));
        assert!(align.plan(2, 20_000_000).reference_step.is_none());
        align.remove(1);
        assert_eq!(align.main, 0);
        align.observe(1, Some(observation(999999.0, 20_000_000, 0.0)), 20_000_000);
        assert!(align.plan(1, 20_000_000).error_ms.is_none());
    }
    #[test]
    fn playback_mapping_accounts_for_prefill_and_clock_frequency_units() {
        let mut tracker = PlaybackTracker::new(48000, 480);
        tracker.record(
            480,
            ReadMapping {
                source_start: 1000.0,
                step: 1.0,
                delay_ms: 20.0,
                valid: true,
            },
        );
        assert!(tracker.observe(240 * 4, 48000 * 4, 10_000_000).is_none());
        let o = tracker.observe(720 * 4, 48000 * 4, 10_100_000).unwrap();
        assert_eq!(o.source_frame, 1240.0);
        assert_eq!(o.applied_delay_ms, 20.0);
        assert!(tracker.observe(720, 0, 10_200_000).is_none());
        assert!(tracker.observe(600 * 4, 48000 * 4, 10_000_001).is_none());
    }
    #[test]
    fn presentation_mapping_includes_fixed_limiter_latency_and_variable_steps() {
        let prefill = 480;
        let limiter_delay = 240;
        let mut tracker = PlaybackTracker::with_processing_delay(48000, prefill, limiter_delay);
        tracker.record(
            480,
            ReadMapping {
                source_start: 1000.0,
                step: 1.0004,
                delay_ms: 20.0,
                valid: true,
            },
        );
        tracker.record(
            480,
            ReadMapping {
                source_start: 1480.192,
                step: 0.9998,
                delay_ms: 20.0,
                valid: true,
            },
        );
        assert!(
            tracker.observe(600, 48000, 10_000_000).is_none(),
            "limiter startup silence has no source mapping"
        );
        let first = tracker.observe(960, 48000, 10_100_000).unwrap();
        assert!((first.source_frame - 1240.096).abs() < 0.00001);
        let second = tracker.observe(1320, 48000, 10_200_000).unwrap();
        assert!((second.source_frame - 1600.168).abs() < 0.00001);
        assert_eq!(second.applied_delay_ms, 20.0);
    }

    #[test]
    fn invalid_transition_segments_are_not_used_for_alignment() {
        let mut tracker = PlaybackTracker::new(48000, 0);
        tracker.record(
            480,
            ReadMapping {
                source_start: 1000.0,
                step: 1.0,
                delay_ms: 20.0,
                valid: false,
            },
        );
        assert!(tracker.observe(240, 48000, 10_000_000).is_none());
        tracker.record(
            480,
            ReadMapping {
                source_start: 1480.0,
                step: 1.0,
                delay_ms: 20.0,
                valid: true,
            },
        );
        assert_eq!(
            tracker
                .observe(720, 48000, 10_100_000)
                .unwrap()
                .source_frame,
            1720.0
        );
    }

    #[test]
    fn removing_slowest_clears_surviving_delay_and_single_output_is_not_calibrating() {
        let mut align = Alignment::new(48000);
        align.register(1);
        align.register(2);
        for i in 0..80 {
            let now = 10_000_000 + i * 100_000;
            let frame = i as f64 * 480.0;
            align.observe(1, Some(observation(frame + 960.0, now, 0.0)), now);
            align.observe(2, Some(observation(frame, now, 0.0)), now);
        }
        assert!(align.plan(1, 17_900_000).auto_delay_ms > 19.0);
        align.remove(2);
        assert_eq!(align.plan(1, 17_900_000).auto_delay_ms, 0.0);
        assert!(!align.plan(1, 17_900_000).measuring);
    }
}
