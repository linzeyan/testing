//! Fixed-concurrency load test: N virtual users send the same request back-to-back until
//! the duration ends. Scripts don't run; this measures the server, not QuickJS.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::Request;
use crate::net::Clients;
use crate::runner;

/// Log-bucketed latency histogram: 1% resolution, fixed ~16 KB no matter how many requests.
const GROWTH: f64 = 1.01;
const BUCKETS: usize = 2048; // 1.01^2048 µs ≈ 7 hours; anything slower lands in the last bucket

pub struct Stats {
    hist: Vec<u64>,
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
    pub statuses: BTreeMap<u16, u64>,
    /// Error message → occurrences; distinct messages are few (refused, timeout, …).
    pub errors: BTreeMap<String, u64>,
    pub started: Instant,
    pub finished: Option<Duration>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            hist: vec![0; BUCKETS],
            count: 0,
            sum_us: 0,
            max_us: 0,
            statuses: BTreeMap::new(),
            errors: BTreeMap::new(),
            started: Instant::now(),
            finished: None,
        }
    }
}

impl Stats {
    fn record(&mut self, us: u64, result: Result<u16, String>) {
        let i = ((us.max(1) as f64).ln() / GROWTH.ln()) as usize;
        self.hist[i.min(BUCKETS - 1)] += 1;
        self.count += 1;
        self.sum_us += us;
        self.max_us = self.max_us.max(us);
        match result {
            Ok(code) => *self.statuses.entry(code).or_default() += 1,
            Err(e) => *self.errors.entry(e).or_default() += 1,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn error_count(&self) -> u64 {
        self.errors.values().sum()
    }

    /// Latency at percentile `p` (0–100), in milliseconds.
    pub fn percentile_ms(&self, p: f64) -> f64 {
        let target = ((p / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, n) in self.hist.iter().enumerate() {
            seen += n;
            if seen >= target {
                return GROWTH.powi(i as i32) / 1000.0;
            }
        }
        0.0
    }
}

/// Runs until `duration` passes; aborting the returned future's task stops every VU, since
/// dropping the JoinSet aborts its tasks.
pub async fn run(
    client: Clients,
    req: Request,
    vus: usize,
    duration: Duration,
    stats: Arc<Mutex<Stats>>,
) {
    let deadline = Instant::now() + duration;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..vus.max(1) {
        let (client, req, stats) = (client.clone(), req.clone(), stats.clone());
        set.spawn(async move {
            while Instant::now() < deadline {
                let t = Instant::now();
                let result = runner::send(&client, req.clone()).await.map(|r| r.status);
                let us = t.elapsed().as_micros() as u64;
                stats.lock().unwrap().record(us, result);
            }
        });
    }
    while set.join_next().await.is_some() {}
    let mut s = stats.lock().unwrap();
    s.finished = Some(s.started.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_within_histogram_resolution() {
        let mut s = Stats::default();
        for ms in 1..=100u64 {
            s.record(ms * 1000, Ok(200));
        }
        s.record(5000, Err("refused".into()));
        for (p, want) in [(50.0, 50.0), (90.0, 90.0), (99.0, 99.0), (100.0, 100.0)] {
            let got = s.percentile_ms(p);
            assert!((got - want).abs() / want < 0.02, "p{p}: {got} vs {want}");
        }
        assert_eq!((s.count, s.error_count(), s.statuses[&200]), (101, 1, 100));
    }

    #[test]
    fn load_run_hits_the_server_from_every_vu() {
        let url = crate::http::tests::echo_server();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap();
        let stats = Arc::new(Mutex::new(Stats::default()));
        let req = Request {
            url,
            ..Default::default()
        };
        rt.block_on(run(
            client,
            req,
            4,
            Duration::from_millis(300),
            stats.clone(),
        ));
        let s = stats.lock().unwrap();
        assert!(s.count >= 4, "{}", s.count);
        assert_eq!(s.statuses.get(&200), Some(&s.count), "{:?}", s.errors);
        assert!(s.finished.is_some());
    }
}
