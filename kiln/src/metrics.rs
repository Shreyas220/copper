//! The last hour of live usage, per instance and per node, in memory only.
//! Nodes report every ten seconds; a restart of kiln simply starts a new
//! hour. Nothing here is the source of truth for anything.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use copper_api::{InstanceId, NodeId, Sample, Usage};

const WINDOW_SECS: i64 = 3600;
/// A little over an hour of ten-second reports.
const MAX_SAMPLES: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subject {
    Instance(InstanceId),
    Node(NodeId),
}

#[derive(Default)]
pub struct Metrics {
    series: Mutex<HashMap<Subject, VecDeque<Sample>>>,
}

impl Metrics {
    pub fn record(&self, subject: Subject, at: i64, usage: Usage) {
        let mut all = self.series.lock().unwrap();
        let series = all.entry(subject).or_default();
        series.push_back(Sample { at, usage });
        while series.len() > MAX_SAMPLES || series.front().is_some_and(|s| s.at < at - WINDOW_SECS) {
            series.pop_front();
        }
    }

    pub fn latest(&self, subject: Subject) -> Option<Usage> {
        self.series.lock().unwrap().get(&subject).and_then(|s| s.back()).map(|s| s.usage)
    }

    pub fn hour(&self, subject: Subject) -> Vec<Sample> {
        self.series.lock().unwrap().get(&subject).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    pub fn forget(&self, subject: Subject) {
        self.series.lock().unwrap().remove(&subject);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_one_hour() {
        let m = Metrics::default();
        let s = Subject::Node(NodeId::nil());
        for t in 0..500 {
            m.record(s, t * 10, Usage { cpu_permille: t as u32, ..Usage::default() });
        }
        let hour = m.hour(s);
        assert_eq!(hour.last().unwrap().at, 4990);
        assert!(hour.first().unwrap().at >= 4990 - WINDOW_SECS);
        assert_eq!(m.latest(s).unwrap().cpu_permille, 499);
    }
}
