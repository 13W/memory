//! Small helpers shared across `bench`, `memory_bench`, and `release_report`
//! (T17-05) — kept in this one crate rather than a shared library crate, the
//! same "small duplication is fine across crates, but not a third copy inside
//! one crate" line `bench::run`/`memory_bench::run` independently drew when
//! each first wrote its own copy of [`percentile`].

/// Nearest-rank percentile over already-collected samples.
pub fn percentile(samples: &mut [f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * q).ceil() as usize;
    samples[idx.saturating_sub(1).min(samples.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank_and_total() {
        let mut one = [7.0];
        assert_eq!(percentile(&mut one, 0.5), 7.0);
        assert_eq!(percentile(&mut one, 0.95), 7.0);

        let mut ten: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        assert_eq!(percentile(&mut ten, 0.50), 5.0);
        assert_eq!(percentile(&mut ten, 0.95), 10.0);

        assert_eq!(percentile(&mut [], 0.5), 0.0, "empty is 0, never NaN");
    }
}
