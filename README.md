# Prop AMM Challenge — σ-Adaptive Blended Curve

**Competition:** [Prop AMM Challenge](https://ammchallenge.com/prop-amm)  
**Author:** [@angry_pacifist](https://x.com/angry__pacifist)  
**AI:** Claude Sonnet 4  
**Current Server Edge:** 458.01 avg / 1000 sims (pending resubmission with blended curve)  
**Local Edge (12-seed validation):** 88.83

---

## What This Is

A custom pricing function for a simulated automated market maker (AMM). The goal: maximize **edge** — the profit your AMM extracts from trading flow relative to a benchmark normalizer AMM.

Your program runs inside a BPF simulation against a benchmark CFMM. Retail traders arrive, arbitrageurs keep prices efficient, and an order router splits flow between the two pools based on who offers better prices. The better your pricing, the more flow you attract and the more edge you earn.

## Strategy Overview

A **σ-adaptive blended curve** that continuously interpolates between two pricing functions based on real-time volatility estimation:

| Regime | σ Range | Curve | Rationale |
|--------|---------|-------|-----------|
| Low volatility | σ ≤ 0.001 | Pure CFMM | Arb risk negligible — maximize retail competitiveness |
| Transition | 0.001 < σ < 0.005 | Linear blend | Progressive arb deterrence as risk increases |
| High volatility | σ ≥ 0.005 | Pure Power-3 (α=2) | Arb deterrence dominates — accept retail rate penalty |

### Key Mechanisms

1. **Blended Curve** — Convex combination of CFMM (`rx·Δ/(ry+Δ)`) and power-3 (`rx·Δ·(Δ+2ry)/(2·(ry+Δ)²)`). Provably concave and monotonic. The blend weight `w_p3` ∈ [0, 10000] adapts per-trade based on EWMA volatility.

2. **EWMA Volatility Estimation** — Tracks `σ²` from arb log-returns with α=0.10 (half-life ~7 trades). Maps σ linearly to both the fee (30–80 bps) and the blend weight.

3. **Directional Fee Skew** — After each arb, applies ±8 bps skew in the arb's direction. Penalizes follow-on trades in the same direction, discounts opposite. Small enough not to hurt retail; large enough to extract marginal edge.

4. **Flow Share Adaptation** — Mild fee adjustment (+1/+3 bps) based on 10-trade windowed flow share EWMA. Only adjusts fees, never the curve.

5. **BPF Compute Optimization** — Three-way branch in `compute_swap` with early exits for pure CFMM (w_p3=0) and pure power-3 (w_p3≥10000) to avoid double u128 computation within the BPF compute budget.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  compute_swap (Blended Curve, u128 integer)          │
│                                                      │
│  1. Read adaptive fee + skew from storage            │
│  2. Read blend weight w_p3 from storage              │
│  3. Apply fee+skew: net = input × (10000-eff)/10k   │
│  4. Curve output (with early-exit optimization):     │
│     IF w_p3 == 0:     pure CFMM                     │
│     ELIF w_p3 >= 10000: pure power-3                 │
│     ELSE: blended (convex combination)               │
│  5. Return min(output, reserve) as u64               │
└──────────────────────────────────────────────────────┘
           ↕ (storage persists between calls)
┌──────────────────────────────────────────────────────┐
│  after_swap (fee + curve adaptation engine)          │
│                                                      │
│  1. Classify trade: arb (new step) vs retail         │
│  2. Track retail count per window                    │
│  3. EWMA vol estimation from arb log-returns         │
│  4. Per-trade σ→fee mapping + flow-share adjustment  │
│  5. Per-trade σ→blend weight (w_p3) update           │
│  6. Per-arb: directional skew (±8 bps)              │
│  7. Write updated state to storage                   │
└──────────────────────────────────────────────────────┘
```

## Score Progression

| Phase | Change | Local Edge | Server Edge |
|-------|--------|:----------:|:-----------:|
| 3 | Hybrid AC+D (initial) | 176 | — |
| 5 | Fixed 40 bps (σ bug fix) | 305 | — |
| 6 | Flow-share windowed adaptation | 364 | — |
| 8 | u128 + adaptive [30-80] bps | ~392 | 398.88 |
| 12 | EWMA α=0.10 + mild flow-share | — | **406.32** |
| 15 | **Power-3 curve (α=2)** | 85.46 | **453.76** |
| 16 | **Directional skew (SKEW=8)** | 87.39 | **458.01** |
| 17 | Per-trade σ-fee update | 87.85 | *pending* |
| **18** | **σ-adaptive blended curve** | **88.83** | ***pending*** |
| 19 | Exhaustive parameter sweep | 88.83 | — |

## Quick Start

```bash
# Validate locally (12 sims)
cargo run -- validate programs/starter/src/lib.rs

# Submit to server
cargo run -- submit programs/starter/src/lib.rs
```

## Files

| Path | Description |
|------|-------------|
| `programs/starter/src/lib.rs` | The AMM pricing strategy (479 lines) |
| `resources/BUILDERS_LOG.md` | Comprehensive builder's log documenting 19 phases of development |
| `crates/` | Simulation framework (normalizer, router, arbitrageur, BPF runner) |

## Builder's Log

The [Builder's Log](resources/BUILDERS_LOG.md) documents the full development journey across 19 phases, including:

- Deep codebase analysis of the simulation framework
- Strategy design (4 candidates evaluated → hybrid selected)
- The power-3 curve breakthrough (+47.4 edge)
- The blended curve innovation (+0.98 edge, required BPF compute fix)
- An exhaustive parameter sweep (14+ experiments, all converged)
- Proof that temporal fee discrimination is architecturally impossible
- Analysis of normalizer/router/arbitrageur mechanics

## Key Learnings

1. **Curve shape >> fee tuning.** Power-3 contributed +47.4 edge. All fee tuning combined contributed ~+12.
2. **Convex blending is free.** A blend of concave functions is provably concave — zero risk, smooth adaptation.
3. **BPF compute budget is real.** Double u128 computation crashes on server. Early exits are essential.
4. **Temporal fee discrimination is impossible.** `compute_swap` has no step/time context — the most powerful lever can't be pulled.
5. **Linear mappings win.** Both σ→fee and σ→blend weight are optimally linear. Non-linear mappings (sqrt, quadratic) consistently perform worse.

---

*Built for the [Prop AMM Challenge](https://ammchallenge.com/prop-amm)*
