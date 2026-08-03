#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct IdlePolicy {
    pub initial_interval: Duration,
    pub strike_interval: Duration,
    pub final_interval: Duration,
    pub cpu_idle_high_percent: f64,
    pub cpu_active_low_percent: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub instance_pid: i32,
    pub sampled_at: Instant,
    pub cpu_usage_usec: u64,
    pub memory_bytes: u64,
    pub process_count: u64,
    pub network_bytes: u64,
    pub latest_network_bytes: u64,
    pub disk_bytes: u64,
    pub latest_disk_bytes: u64,
    pub established_tcp_connections: u64,
    pub total_connections: u64,
    pub vcpu_count: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct Activity {
    pub active: bool,
    pub counters_regressed: bool,
    pub cpu_percent: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDecision {
    None,
    ActivityReset,
    Strike(u8),
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub sample: Sample,
    pub decision: IdleDecision,
    pub activity: Activity,
    pub last_activity: Instant,
}

#[derive(Debug, Clone, Copy)]
pub struct Monitor {
    pub last_sample: Sample,
    pub cpu_percent: Option<f64>,
    pub last_activity: Instant,
    pub strikes: u8,
    pub next_check: Instant,
}

impl IdlePolicy {
    #[must_use]
    pub fn cpu_threshold_percent(self) -> f64 {
        self.cpu_idle_high_percent
            + 0.2 * (self.cpu_active_low_percent - self.cpu_idle_high_percent)
    }
}

impl Monitor {
    pub fn new(sample: Sample, policy: IdlePolicy) -> Self {
        Self {
            last_sample: sample,
            cpu_percent: None,
            last_activity: sample.sampled_at,
            strikes: 0,
            next_check: sample.sampled_at + policy.initial_interval,
        }
    }

    pub fn observe(&mut self, sample: Sample, policy: IdlePolicy) -> (IdleDecision, Activity) {
        let activity = sample.activity_since(self.last_sample, policy);
        self.last_sample = sample;
        self.cpu_percent = activity.cpu_percent;
        if activity.active {
            self.last_activity = sample.sampled_at;
            self.strikes = 0;
            self.next_check = sample.sampled_at + policy.initial_interval;
            return (IdleDecision::ActivityReset, activity);
        }
        if sample.sampled_at < self.next_check {
            return (IdleDecision::None, activity);
        }
        if self.strikes < 3 {
            self.strikes += 1;
            self.next_check = sample.sampled_at
                + if self.strikes == 3 {
                    policy.final_interval
                } else {
                    policy.strike_interval
                };
            (IdleDecision::Strike(self.strikes), activity)
        } else {
            (IdleDecision::Shutdown, activity)
        }
    }

    pub fn observation(&mut self, sample: Sample, policy: IdlePolicy) -> Observation {
        let (decision, activity) = self.observe(sample, policy);
        Observation {
            sample,
            decision,
            activity,
            last_activity: self.last_activity,
        }
    }
}

impl Sample {
    pub(crate) fn cpu_percent_since(self, previous: Self) -> Option<f64> {
        let elapsed = self
            .sampled_at
            .checked_duration_since(previous.sampled_at)?;
        let usage =
            Duration::from_micros(self.cpu_usage_usec.checked_sub(previous.cpu_usage_usec)?);
        (!elapsed.is_zero() && self.vcpu_count != 0).then(|| {
            100.0 * usage.as_secs_f64() / (elapsed.as_secs_f64() * f64::from(self.vcpu_count))
        })
    }

    fn activity_since(self, previous: Self, policy: IdlePolicy) -> Activity {
        let counters_regressed = self.instance_pid != previous.instance_pid
            || self.cpu_usage_usec < previous.cpu_usage_usec
            || self.network_bytes < previous.network_bytes
            || self.disk_bytes < previous.disk_bytes;
        let cpu_percent = self.cpu_percent_since(previous);
        Activity {
            active: counters_regressed
                || cpu_percent.is_some_and(|cpu| cpu > policy.cpu_threshold_percent())
                || self.network_bytes > previous.network_bytes
                || self.disk_bytes > previous.disk_bytes
                || self.process_count != previous.process_count
                || self.established_tcp_connections > 0,
            counters_regressed,
            cpu_percent,
        }
    }
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            initial_interval: Duration::from_mins(5),
            strike_interval: Duration::from_mins(1),
            final_interval: Duration::from_secs(30),
            cpu_idle_high_percent: 0.5,
            cpu_active_low_percent: 3.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type SampleCase = (&'static str, fn(&mut Sample));

    fn sample(origin: Instant, at: u64) -> Sample {
        Sample {
            instance_pid: 10,
            sampled_at: origin + Duration::from_secs(at),
            cpu_usage_usec: 0,
            memory_bytes: 100,
            process_count: 1,
            network_bytes: 0,
            latest_network_bytes: 0,
            disk_bytes: 0,
            latest_disk_bytes: 0,
            established_tcp_connections: 0,
            total_connections: 0,
            vcpu_count: 1,
        }
    }

    #[test]
    fn cpu_threshold_and_vcpu_normalization_match_the_specification() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let old = sample(origin, 0);
        let mut new = sample(origin, 60);
        new.cpu_usage_usec = 600_000;

        assert!((policy.cpu_threshold_percent() - 1.0).abs() < f64::EPSILON);
        assert_eq!(new.cpu_percent_since(old), Some(1.0));
        assert!(!new.activity_since(old, policy).active);

        new.vcpu_count = 2;
        assert_eq!(new.cpu_percent_since(old), Some(0.5));
    }

    #[test]
    fn monitor_retains_nullable_cpu_calculations() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let first = sample(origin, 0);
        let mut monitor = Monitor::new(first, policy);
        assert_eq!(monitor.cpu_percent, None);

        let mut second = first;
        second.sampled_at = origin + Duration::from_secs(60);
        second.cpu_usage_usec = 600_000;
        let (_, activity) = monitor.observe(second, policy);
        assert_eq!(activity.cpu_percent, Some(1.0));
        assert_eq!(monitor.cpu_percent, Some(1.0));

        let mut reset = second;
        reset.sampled_at = origin + Duration::from_secs(61);
        reset.cpu_usage_usec = 1;
        let (_, activity) = monitor.observe(reset, policy);
        assert_eq!(activity.cpu_percent, None);
        assert_eq!(monitor.cpu_percent, None);
    }

    #[test]
    fn idle_comparisons_use_accumulated_bytes_not_latest_intervals() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let mut monitor = Monitor::new(sample(origin, 0), policy);
        let mut first = sample(origin, 60);
        first.network_bytes = 100;
        first.latest_network_bytes = 100;
        assert_eq!(
            monitor.observe(first, policy).0,
            IdleDecision::ActivityReset
        );

        let mut next = first;
        next.sampled_at = origin + Duration::from_secs(120);
        next.latest_network_bytes = 1;
        next.network_bytes = 101;
        assert_eq!(monitor.observe(next, policy).0, IdleDecision::ActivityReset);

        let mut no_new_bytes = next;
        no_new_bytes.sampled_at = origin + Duration::from_secs(180);
        no_new_bytes.latest_network_bytes = 0;
        assert_eq!(monitor.observe(no_new_bytes, policy).0, IdleDecision::None);
    }

    #[test]
    fn idle_flow_waits_between_all_three_strikes_and_shutdown() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let mut monitor = Monitor::new(sample(origin, 0), policy);
        let checkpoints = [
            (299, IdleDecision::None),
            (300, IdleDecision::Strike(1)),
            (359, IdleDecision::None),
            (360, IdleDecision::Strike(2)),
            (419, IdleDecision::None),
            (420, IdleDecision::Strike(3)),
            (449, IdleDecision::None),
            (450, IdleDecision::Shutdown),
        ];

        for (at, expected) in checkpoints {
            let observation = monitor.observation(sample(origin, at), policy);
            assert_eq!(observation.decision, expected, "decision at {at}s");
            assert_eq!(observation.last_activity, origin);
        }
    }

    #[test]
    fn every_configured_activity_signal_resets_strikes_and_last_activity() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let signals: [SampleCase; 5] = [
            ("CPU above threshold", |sample| {
                sample.cpu_usage_usec = 20_000;
            }),
            ("network traffic", |sample| sample.network_bytes = 1),
            ("disk traffic", |sample| sample.disk_bytes = 1),
            ("process-count change", |sample| sample.process_count = 2),
            ("established TCP connection", |sample| {
                sample.established_tcp_connections = 1;
            }),
        ];

        for (name, signal) in signals {
            let mut monitor = Monitor::new(sample(origin, 0), policy);
            assert_eq!(
                monitor.observe(sample(origin, 300), policy).0,
                IdleDecision::Strike(1)
            );
            let mut active = sample(origin, 301);
            signal(&mut active);

            let (decision, activity) = monitor.observe(active, policy);

            assert!(activity.active, "{name} was not classified as activity");
            assert_eq!(decision, IdleDecision::ActivityReset, "{name}");
            assert_eq!(monitor.strikes, 0, "{name}");
            assert_eq!(monitor.last_activity, active.sampled_at, "{name}");
        }
    }

    #[test]
    fn health_metrics_without_activity_do_not_reset_the_idle_flow() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let mut monitor = Monitor::new(sample(origin, 0), policy);
        assert_eq!(
            monitor.observe(sample(origin, 300), policy).0,
            IdleDecision::Strike(1)
        );
        let mut health_only = sample(origin, 301);
        health_only.memory_bytes = 200;
        health_only.total_connections = 1;

        let (decision, activity) = monitor.observe(health_only, policy);

        assert!(!activity.active);
        assert_eq!(decision, IdleDecision::None);
        assert_eq!(monitor.strikes, 1);
        assert_eq!(monitor.last_activity, origin);
    }

    #[test]
    fn a_new_instance_or_regressed_counter_establishes_a_fresh_baseline() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let regressions: [SampleCase; 4] = [
            ("instance PID", |sample| sample.instance_pid = 11),
            ("CPU counter", |sample| sample.cpu_usage_usec = 9),
            ("network counter", |sample| sample.network_bytes = 9),
            ("disk counter", |sample| sample.disk_bytes = 9),
        ];

        for (name, regress) in regressions {
            let mut baseline = sample(origin, 0);
            baseline.cpu_usage_usec = 10;
            baseline.network_bytes = 10;
            baseline.disk_bytes = 10;
            let mut monitor = Monitor::new(baseline, policy);
            let mut regressed = baseline;
            regressed.sampled_at = origin + Duration::from_secs(1);
            regress(&mut regressed);

            let (decision, activity) = monitor.observe(regressed, policy);

            assert!(activity.active, "{name} did not establish a baseline");
            assert!(activity.counters_regressed, "{name}");
            assert_eq!(decision, IdleDecision::ActivityReset, "{name}");
            assert_eq!(monitor.last_sample, regressed, "{name}");

            let mut stable = regressed;
            stable.sampled_at = origin + Duration::from_secs(2);
            let (decision, activity) = monitor.observe(stable, policy);
            assert!(!activity.active, "{name} was not accepted as the baseline");
            assert!(!activity.counters_regressed, "{name}");
            assert_eq!(decision, IdleDecision::None, "{name}");
        }
    }
}
