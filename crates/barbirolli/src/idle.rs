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
    pub disk_bytes: u64,
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
            last_activity: sample.sampled_at,
            strikes: 0,
            next_check: sample.sampled_at + policy.initial_interval,
        }
    }

    pub fn observe(&mut self, sample: Sample, policy: IdlePolicy) -> (IdleDecision, Activity) {
        let activity = sample.activity_since(self.last_sample, policy);
        self.last_sample = sample;
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
    fn cpu_percent_since(self, previous: Self) -> Option<f64> {
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

    fn sample(origin: Instant, at: u64, cpu: u64) -> Sample {
        Sample {
            instance_pid: 10,
            sampled_at: origin + Duration::from_secs(at),
            cpu_usage_usec: cpu,
            memory_bytes: 100,
            process_count: 1,
            network_bytes: 0,
            disk_bytes: 0,
            established_tcp_connections: 0,
            total_connections: 0,
            vcpu_count: 1,
        }
    }

    #[test]
    fn threshold_defaults_to_one_percent() {
        assert!((IdlePolicy::default().cpu_threshold_percent() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cpu_is_normalized_by_elapsed_time_and_vcpus() {
        let origin = Instant::now();
        let old = sample(origin, 0, 0);
        let mut new = sample(origin, 60, 600_000);
        assert_eq!(new.cpu_percent_since(old), Some(1.0));
        new.vcpu_count = 2;
        assert_eq!(new.cpu_percent_since(old), Some(0.5));
    }

    #[test]
    fn fixed_timeline_reaches_shutdown() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let mut monitor = Monitor::new(sample(origin, 0, 0), policy);
        assert_eq!(
            monitor.observe(sample(origin, 300, 0), policy).0,
            IdleDecision::Strike(1)
        );
        assert_eq!(
            monitor.observe(sample(origin, 360, 0), policy).0,
            IdleDecision::Strike(2)
        );
        assert_eq!(
            monitor.observe(sample(origin, 420, 0), policy).0,
            IdleDecision::Strike(3)
        );
        let observation = monitor.observation(sample(origin, 450, 0), policy);
        assert_eq!(observation.decision, IdleDecision::Shutdown);
        assert_eq!(
            observation.sample.sampled_at,
            origin + Duration::from_secs(450)
        );
        assert_eq!(observation.last_activity, origin);
    }

    #[test]
    fn activity_resets_but_memory_does_not() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let mut monitor = Monitor::new(sample(origin, 0, 0), policy);
        monitor.observe(sample(origin, 300, 0), policy);
        let mut memory = sample(origin, 301, 0);
        memory.memory_bytes = 200;
        assert_eq!(monitor.observe(memory, policy).0, IdleDecision::None);
        let mut traffic = sample(origin, 302, 0);
        traffic.memory_bytes = 200;
        traffic.network_bytes = 1;
        assert_eq!(
            monitor.observe(traffic, policy).0,
            IdleDecision::ActivityReset
        );
        assert_eq!(monitor.strikes, 0);
    }

    #[test]
    fn established_connection_is_always_active() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let baseline = sample(origin, 0, 0);
        let mut connected = sample(origin, 1, 0);
        connected.established_tcp_connections = 1;
        assert!(connected.activity_since(baseline, policy).active);
    }

    #[test]
    fn counter_regression_resets_the_baseline() {
        let origin = Instant::now();
        let policy = IdlePolicy::default();
        let activity = sample(origin, 1, 0).activity_since(sample(origin, 0, 10), policy);
        assert!(activity.active);
        assert!(activity.counters_regressed);
    }
}
