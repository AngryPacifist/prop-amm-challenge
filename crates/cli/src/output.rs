use prop_amm_shared::result::BatchResult;
use std::time::Duration;

pub struct RunTimings {
    pub compile_or_load: Duration,
    pub simulation: Duration,
    pub total: Duration,
}

pub fn print_results(result: &BatchResult, timings: RunTimings) {
    let seed_range = result
        .results
        .iter()
        .map(|r| r.seed)
        .fold(None::<(u64, u64)>, |acc, seed| match acc {
            Some((lo, hi)) => Some((lo.min(seed), hi.max(seed))),
            None => Some((seed, seed)),
        });

    println!("\n========================================");
    println!("  Simulations: {}", result.n_sims());
    if let Some((seed_start, seed_end)) = seed_range {
        println!("  Seed range:  {}..={}", seed_start, seed_end);
    }
    println!(
        "  Compile/load:{:>8.2}s",
        timings.compile_or_load.as_secs_f64()
    );
    println!("  Simulation:  {:>8.2}s", timings.simulation.as_secs_f64());
    println!("  Total:       {:>8.2}s", timings.total.as_secs_f64());
    println!("  Avg edge:    {:.2}", result.avg_edge());
    println!("  Total edge:  {:.2}", result.total_edge);

    // Per-seed edge diagnostics with hyperparameters
    let variance = prop_amm_shared::config::HyperparameterVariance::default();
    let base = prop_amm_shared::config::SimulationConfig::default();
    
    println!("");
    println!(
        "  {:<8} {:>10} {:>8} {:>8} {:>8} {:>8} {:>10}",
        "seed", "edge", "gbm_sig", "arr_rt", "mean_sz", "n_fee", "n_liq"
    );
    println!("  {}", "-".repeat(70));
    
    let mut negative_count = 0;
    let mut edges: Vec<f64> = Vec::new();
    for r in &result.results {
        let config = variance.apply(&base, r.seed);
        let marker = if r.submission_edge < 0.0 { " <<<" } else { "" };
        if r.submission_edge < 0.0 { negative_count += 1; }
        edges.push(r.submission_edge);
        println!(
            "  {:<8} {:>10.3} {:>8.5} {:>8.3} {:>8.1} {:>8} {:>10.3}{}",
            r.seed, r.submission_edge, config.gbm_sigma,
            config.retail_arrival_rate, config.retail_mean_size,
            config.norm_fee_bps, config.norm_liquidity_mult, marker,
        );
    }
    edges.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("  {}", "-".repeat(70));
    println!("  Negative: {}/{}  |  Min: {:.3}  |  Max: {:.3}",
        negative_count, result.n_sims(),
        edges.first().copied().unwrap_or(0.0),
        edges.last().copied().unwrap_or(0.0));
    if edges.len() >= 4 {
        let q1 = edges[edges.len() / 4];
        let median = edges[edges.len() / 2];
        let q3 = edges[3 * edges.len() / 4];
        println!("  Q1: {:.3}  |  Median: {:.3}  |  Q3: {:.3}", q1, median, q3);
    }

    println!("========================================");

    if let Some(stats) = prop_amm_sim::search_stats::snapshot_if_enabled() {
        let arb_calls = stats.arb_golden_calls.max(1);
        let router_calls = stats.router_calls.max(1);
        println!("\nSearch stats (PROP_AMM_SEARCH_STATS=1):");
        println!(
            "  Arb golden:  calls={} iters={} (avg {:.2}/call) evals={} (avg {:.2}/call) early_stop_amount_tol={}",
            stats.arb_golden_calls,
            stats.arb_golden_iters,
            stats.arb_golden_iters as f64 / arb_calls as f64,
            stats.arb_golden_evals,
            stats.arb_golden_evals as f64 / arb_calls as f64,
            stats.arb_early_stop_amount_tol,
        );
        println!(
            "  Arb bracket: calls={} evals={} (avg {:.2}/call)",
            stats.arb_bracket_calls,
            stats.arb_bracket_evals,
            stats.arb_bracket_evals as f64 / stats.arb_bracket_calls.max(1) as f64,
        );
        println!(
            "  Router:     calls={} iters={} (avg {:.2}/call) evals={} (avg {:.2}/call) early_stop_rel_gap={}",
            stats.router_calls,
            stats.router_golden_iters,
            stats.router_golden_iters as f64 / router_calls as f64,
            stats.router_evals,
            stats.router_evals as f64 / router_calls as f64,
            stats.router_early_stop_rel_gap,
        );
    }
}
