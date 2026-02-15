#[cfg(not(feature = "no-entrypoint"))]
use pinocchio::entrypoint;
use pinocchio::{account_info::AccountInfo, pubkey::Pubkey, ProgramResult};
use prop_amm_submission_sdk::{set_return_data_bytes, set_return_data_u64, set_storage};

const NAME: &str = "You can just do things";
const MODEL_USED: &str = "Claude Sonnet 4"; // AI-assisted strategy design

// ─── Storage layout ──────────────────────────────────────────────────────
// Offset  Size  Type   Field
// 0       4     u32    trade_count      (total trades since start)
// 4       4     u32    retail_count     (retail trades in current window)
// 8       8     f64    sigma_sq_ewma    (EWMA of σ², exponentially weighted)
// 16      4     u32    n_vol_samples    (arb observations for EWMA warmup)
// 20      4     u32    (reserved)
// 24      8     u64    last_step
// 32      2     u16    current_fee_bps
// 34      8     f64    last_log_price   (log(ry/rx) from previous arb trade)
// 42      4     u32    window_trade_count (trades in current fee-update window)
// 46      2     i16    skew_bps         (directional fee skew)
// 48      8     f64    flow_share_ewma  (smoothed retail flow share)
// 56      1     u8     current_alpha    (curve power: 1=CFMM, 2=power-3)
const STORAGE_SIZE: usize = 1024;

// ─── Calibration constants ─────────────────────────────────────────────
const COLD_START_FEE_BPS: u64 = 40;
const MIN_FEE_BPS: u16 = 30;
const MAX_FEE_BPS: u16 = 80;
const VOL_WINDOW_MIN: u32 = 10;
const FEE_UPDATE_INTERVAL: u32 = 10;
const EWMA_ALPHA: f64 = 0.10;
const SIGMA_LOW: f64 = 0.0001;
const SIGMA_HIGH: f64 = 0.007;
const SKEW_BPS: i16 = 8;  // Directional fee skew: penalize arb's direction, discount opposite
const FLOW_SHARE_EWMA_ALPHA: f64 = 0.15; // Smoothing for flow share estimation
const SIGMA_ALPHA_THRESHOLD: f64 = 0.001; // Below this σ, use CFMM (α=1) for competitiveness

// ─── Deserialization via wincode ──────────────────────────────────────────
#[derive(wincode::SchemaRead)]
struct ComputeSwapInstruction {
    side: u8,
    input_amount: u64,
    reserve_x: u64,
    reserve_y: u64,
    _storage: [u8; STORAGE_SIZE],
}

// ─── BPF entrypoint ──────────────────────────────────────────────────────
#[cfg(not(feature = "no-entrypoint"))]
entrypoint!(process_instruction);

pub fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.is_empty() {
        return Ok(());
    }

    match instruction_data[0] {
        0 | 1 => {
            let output = compute_swap(instruction_data);
            set_return_data_u64(output);
        }
        2 => {
            after_swap_bpf(instruction_data);
        }
        3 => set_return_data_bytes(NAME.as_bytes()),
        4 => set_return_data_bytes(get_model_used().as_bytes()),
        _ => {}
    }

    Ok(())
}

pub fn get_model_used() -> &'static str {
    MODEL_USED
}

// ═══════════════════════════════════════════════════════════════════════════
//  COMPUTE SWAP — Standard CFMM with f64 pipeline
// ═══════════════════════════════════════════════════════════════════════════

pub fn compute_swap(data: &[u8]) -> u64 {
    let decoded: ComputeSwapInstruction = match wincode::deserialize(data) {
        Ok(decoded) => decoded,
        Err(_) => return 0,
    };

    let side = decoded.side;

    if decoded.reserve_x == 0 || decoded.reserve_y == 0 || decoded.input_amount == 0 {
        return 0;
    }

    // Read fee, directional skew, and curve parameter from storage
    let storage = &data[25..];
    let trade_count = read_u32(storage, 0);
    let stored_fee_bps = read_u16(storage, 32);
    let skew_raw = read_i16(storage, 46);  // Directional skew set by afterSwap
    let alpha_raw = read_u8(storage, 56);  // Curve power parameter

    let base_fee_bps: u16 = if trade_count == 0 || stored_fee_bps == 0 {
        COLD_START_FEE_BPS as u16
    } else {
        clamp_u16(stored_fee_bps, MIN_FEE_BPS, MAX_FEE_BPS)
    };

    // Apply directional skew: positive skew = buy-X costs more, sell-X costs less
    // side 0 = buy X (add skew), side 1 = sell X (subtract skew)
    let skew_sign: i32 = if side == 0 { 1 } else { -1 };
    let effective_fee = (base_fee_bps as i32 + skew_raw as i32 * skew_sign)
        .max(MIN_FEE_BPS as i32)
        .min(MAX_FEE_BPS as i32) as u16;

    // Blended curve weight: 0=pure CFMM, 10000=pure power-3
    let w_p3_raw = read_u16(storage, 56);
    let w_p3: u128 = if trade_count == 0 && w_p3_raw == 0 {
        10000 // Cold start: conservative power-3
    } else if w_p3_raw > 10000 {
        10000
    } else {
        w_p3_raw as u128
    };

    let input = decoded.input_amount as u128;
    let rx = decoded.reserve_x as u128;
    let ry = decoded.reserve_y as u128;
    if rx == 0 || ry == 0 { return 0; }

    let fee_num = (10000u128).saturating_sub(effective_fee as u128);
    let fee_den = 10000u128;

    match side {
        0 => {
            let net = input.saturating_mul(fee_num) / fee_den;
            if net == 0 { return 0; }
            let sum = ry.saturating_add(net);
            if w_p3 >= 10000 {
                // Pure power-3 (most sims): avoid computing CFMM
                let sum_sq = sum.saturating_mul(sum);
                let n2ry = net.saturating_add(ry.saturating_mul(2));
                let output = rx.saturating_mul(net).saturating_mul(n2ry) / (2u128.saturating_mul(sum_sq));
                output.min(rx) as u64
            } else if w_p3 == 0 {
                // Pure CFMM: avoid computing power-3
                let output = rx.saturating_mul(net) / sum;
                output.min(rx) as u64
            } else {
                // Blend: compute both and mix
                let out_cfmm = rx.saturating_mul(net) / sum;
                let sum_sq = sum.saturating_mul(sum);
                let n2ry = net.saturating_add(ry.saturating_mul(2));
                let out_p3 = rx.saturating_mul(net).saturating_mul(n2ry) / (2u128.saturating_mul(sum_sq));
                let w_cfmm = 10000u128 - w_p3;
                let output = (w_p3.saturating_mul(out_p3) + w_cfmm.saturating_mul(out_cfmm)) / 10000;
                output.min(rx) as u64
            }
        }
        1 => {
            let net = input.saturating_mul(fee_num) / fee_den;
            if net == 0 { return 0; }
            let sum = rx.saturating_add(net);
            if w_p3 >= 10000 {
                let sum_sq = sum.saturating_mul(sum);
                let n2rx = net.saturating_add(rx.saturating_mul(2));
                let output = ry.saturating_mul(net).saturating_mul(n2rx) / (2u128.saturating_mul(sum_sq));
                output.min(ry) as u64
            } else if w_p3 == 0 {
                let output = ry.saturating_mul(net) / sum;
                output.min(ry) as u64
            } else {
                let out_cfmm = ry.saturating_mul(net) / sum;
                let sum_sq = sum.saturating_mul(sum);
                let n2rx = net.saturating_add(rx.saturating_mul(2));
                let out_p3 = ry.saturating_mul(net).saturating_mul(n2rx) / (2u128.saturating_mul(sum_sq));
                let w_cfmm = 10000u128 - w_p3;
                let output = (w_p3.saturating_mul(out_p3) + w_cfmm.saturating_mul(out_cfmm)) / 10000;
                output.min(ry) as u64
            }
        }
        _ => 0,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  AFTER SWAP — Volatility estimation and fee adaptation
// ═══════════════════════════════════════════════════════════════════════════

fn after_swap_bpf(data: &[u8]) {
    if data.len() < 42 + STORAGE_SIZE {
        return;
    }
    let mut storage_buf = [0u8; STORAGE_SIZE];
    let src = &data[42..42 + STORAGE_SIZE];
    storage_buf.copy_from_slice(src);
    after_swap_inner(data, &mut storage_buf);
    let _ = set_storage(&storage_buf);
}

pub fn after_swap(data: &[u8], storage: &mut [u8]) {
    after_swap_inner(data, storage);
}

fn after_swap_inner(data: &[u8], storage: &mut [u8]) {
    if data.len() < 42 {
        return;
    }

    // ─── Parse after_swap data ───
    // data[0] = tag (2)
    // data[1] = side
    // data[2..10] = input_amount (u64)
    // data[10..18] = output_amount (u64)
    // data[18..26] = reserve_x (u64, post-trade)
    // data[26..34] = reserve_y (u64, post-trade)
    // data[34..42] = step (u64)
    let reserve_x = u64::from_le_bytes([
        data[18], data[19], data[20], data[21], data[22], data[23], data[24], data[25],
    ]);
    let reserve_y = u64::from_le_bytes([
        data[26], data[27], data[28], data[29], data[30], data[31], data[32], data[33],
    ]);
    let step = u64::from_le_bytes([
        data[34], data[35], data[36], data[37], data[38], data[39], data[40], data[41],
    ]);

    // ─── Read state ───
    let trade_count = read_u32(storage, 0);
    let mut retail_count = read_u32(storage, 4);
    // (sigma_sq_ewma is read later at line 221, not here)
    let mut n_samples = read_u32(storage, 16);
    let last_step = read_u64(storage, 24);
    let current_fee_bps = read_u16(storage, 32);
    let last_log_price = read_f64(storage, 34);
    let mut window_trade_count = read_u32(storage, 42);
    let trade_side = data[1];  // 0 = buy X, 1 = sell X

    // ─── Classify: arb vs retail ───
    let is_new_step = step > last_step;
    let is_likely_arb = is_new_step && trade_count > 0;

    // ─── Flow tracking ───
    if !is_likely_arb || trade_count == 0 {
        retail_count = retail_count.saturating_add(1);
    }
    window_trade_count = window_trade_count.saturating_add(1);

    // ─── Volatility estimation via EWMA ───
    // Track log(ry/rx) at each arb trade (new step). Arb trades realign
    // price closer to fair, so step-over-step changes approximate σ.
    // EWMA: σ²_new = α × return² + (1−α) × σ²_old  (responds faster than batch avg)
    let current_log_price = if reserve_x > 0 && reserve_y > 0 {
        (reserve_y as f64 / reserve_x as f64).ln()
    } else {
        0.0
    };

    let mut sigma_sq_ewma = read_f64(storage, 8);

    if is_likely_arb && last_log_price != 0.0 && current_log_price.is_finite() && last_log_price.is_finite() {
        let log_return = current_log_price - last_log_price;
        if log_return.is_finite() {
            let return_sq = log_return * log_return;
            if n_samples == 0 {
                // First observation — initialize EWMA to this value
                sigma_sq_ewma = return_sq;
            } else {
                sigma_sq_ewma = EWMA_ALPHA * return_sq + (1.0 - EWMA_ALPHA) * sigma_sq_ewma;
            }
            n_samples = n_samples.saturating_add(1);
        }
    }

    // ─── Read flow share EWMA ───
    let mut flow_share_ewma = read_f64(storage, 48);

    // ─── Fee update: σ-baseline per-trade + bidirectional flow-share adjustment ───
    let new_fee_bps = if n_samples >= VOL_WINDOW_MIN && sigma_sq_ewma.is_finite() && sigma_sq_ewma > 0.0 {
        // σ-based baseline — always use the latest σ_ewma
        let sigma_est = sigma_sq_ewma.sqrt();
        let t = if sigma_est <= SIGMA_LOW {
            0.0
        } else if sigma_est >= SIGMA_HIGH {
            1.0
        } else {
            (sigma_est - SIGMA_LOW) / (SIGMA_HIGH - SIGMA_LOW)
        };
        let base_fee = (MIN_FEE_BPS as f64 + t * (MAX_FEE_BPS - MIN_FEE_BPS) as f64).round() as i32;

        // Bidirectional flow-share adjustment (EWMA-smoothed, applied at window boundary)
        let adj: i32 = if window_trade_count >= FEE_UPDATE_INTERVAL {
            let flow_share = if window_trade_count > 0 {
                retail_count as f64 / window_trade_count as f64
            } else {
                0.5
            };
            // Update EWMA of flow share
            flow_share_ewma = if flow_share_ewma <= 0.0 || !flow_share_ewma.is_finite() {
                flow_share  // Initialize on first window
            } else {
                FLOW_SHARE_EWMA_ALPHA * flow_share + (1.0 - FLOW_SHARE_EWMA_ALPHA) * flow_share_ewma
            };
            window_trade_count = 0;
            retail_count = 0;
            // Mild upward-only flow-share tiers (aggressive tiers hurt when combined with α-adaptive)
            if flow_share > 0.55 { 3 } else if flow_share > 0.45 { 1 } else { 0 }
        } else {
            0 // Between windows: pure σ-baseline (proven best in per-trade testing)
        };
        let adjusted = (base_fee + adj).max(MIN_FEE_BPS as i32).min(MAX_FEE_BPS as i32);
        adjusted as u16
    } else if current_fee_bps == 0 {
        COLD_START_FEE_BPS as u16
    } else {
        // During warmup: keep cold start or current, still reset window counters
        if window_trade_count >= FEE_UPDATE_INTERVAL {
            window_trade_count = 0;
            retail_count = 0;
        }
        if current_fee_bps == 0 { COLD_START_FEE_BPS as u16 } else { current_fee_bps }
    };

    // ─── σ-adaptive blend weight: continuous between CFMM and power-3 ───
    // w_p3: 0 = pure CFMM, 10000 = pure power-3
    let new_w_p3: u16 = if n_samples >= VOL_WINDOW_MIN && sigma_sq_ewma.is_finite() && sigma_sq_ewma > 0.0 {
        let sigma_est = sigma_sq_ewma.sqrt();
        if sigma_est <= 0.001 {
            0
        } else if sigma_est >= 0.005 {
            10000
        } else {
            let t = (sigma_est - 0.001) / (0.005 - 0.001);
            (t * 10000.0).round().max(0.0).min(10000.0) as u16
        }
    } else {
        10000 // Cold start: conservative
    };

    // ─── Directional skew: after arb, penalize same direction ───
    let new_skew: i16 = if is_likely_arb {
        if trade_side == 0 { SKEW_BPS } else { -SKEW_BPS }
    } else {
        read_i16(storage, 46)  // keep previous skew
    };

    // ─── Update last_log_price for vol tracking ───
    let new_log_price = if is_likely_arb && current_log_price.is_finite() {
        current_log_price
    } else if trade_count == 0 && current_log_price.is_finite() {
        current_log_price
    } else {
        last_log_price
    };

    // ─── Write state ───
    write_u32(storage, 0, trade_count.saturating_add(1));
    write_u32(storage, 4, retail_count);
    write_f64(storage, 8, sigma_sq_ewma);
    write_u32(storage, 16, n_samples);
    write_u64(storage, 24, step);
    write_u16(storage, 32, new_fee_bps);
    write_f64(storage, 34, new_log_price);
    write_u32(storage, 42, window_trade_count);
    write_i16(storage, 46, new_skew);
    write_f64(storage, 48, flow_share_ewma);
    write_u16(storage, 56, new_w_p3);
}

// ═══════════════════════════════════════════════════════════════════════════
//  FEE COMPUTATION — removed, now inline in after_swap_inner
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
//  STORAGE HELPERS
// ═══════════════════════════════════════════════════════════════════════════

fn read_u8(storage: &[u8], offset: usize) -> u8 {
    if offset >= storage.len() { return 0; }
    storage[offset]
}

fn read_u16(storage: &[u8], offset: usize) -> u16 {
    if offset + 2 > storage.len() { return 0; }
    u16::from_le_bytes([storage[offset], storage[offset + 1]])
}

fn read_i16(storage: &[u8], offset: usize) -> i16 {
    if offset + 2 > storage.len() { return 0; }
    i16::from_le_bytes([storage[offset], storage[offset + 1]])
}

fn read_u32(storage: &[u8], offset: usize) -> u32 {
    if offset + 4 > storage.len() { return 0; }
    u32::from_le_bytes([
        storage[offset], storage[offset + 1],
        storage[offset + 2], storage[offset + 3],
    ])
}

fn read_u64(storage: &[u8], offset: usize) -> u64 {
    if offset + 8 > storage.len() { return 0; }
    u64::from_le_bytes([
        storage[offset], storage[offset + 1],
        storage[offset + 2], storage[offset + 3],
        storage[offset + 4], storage[offset + 5],
        storage[offset + 6], storage[offset + 7],
    ])
}

fn read_f64(storage: &[u8], offset: usize) -> f64 {
    if offset + 8 > storage.len() { return 0.0; }
    f64::from_le_bytes([
        storage[offset], storage[offset + 1],
        storage[offset + 2], storage[offset + 3],
        storage[offset + 4], storage[offset + 5],
        storage[offset + 6], storage[offset + 7],
    ])
}

fn write_u8(storage: &mut [u8], offset: usize, val: u8) {
    if offset >= storage.len() { return; }
    storage[offset] = val;
}

fn write_u16(storage: &mut [u8], offset: usize, val: u16) {
    if offset + 2 > storage.len() { return; }
    let bytes = val.to_le_bytes();
    storage[offset] = bytes[0];
    storage[offset + 1] = bytes[1];
}

fn write_i16(storage: &mut [u8], offset: usize, val: i16) {
    if offset + 2 > storage.len() { return; }
    let bytes = val.to_le_bytes();
    storage[offset] = bytes[0];
    storage[offset + 1] = bytes[1];
}

fn write_u32(storage: &mut [u8], offset: usize, val: u32) {
    if offset + 4 > storage.len() { return; }
    let bytes = val.to_le_bytes();
    storage[offset] = bytes[0];
    storage[offset + 1] = bytes[1];
    storage[offset + 2] = bytes[2];
    storage[offset + 3] = bytes[3];
}

fn write_u64(storage: &mut [u8], offset: usize, val: u64) {
    if offset + 8 > storage.len() { return; }
    let bytes = val.to_le_bytes();
    storage[offset] = bytes[0];
    storage[offset + 1] = bytes[1];
    storage[offset + 2] = bytes[2];
    storage[offset + 3] = bytes[3];
    storage[offset + 4] = bytes[4];
    storage[offset + 5] = bytes[5];
    storage[offset + 6] = bytes[6];
    storage[offset + 7] = bytes[7];
}

fn write_f64(storage: &mut [u8], offset: usize, val: f64) {
    if offset + 8 > storage.len() { return; }
    let bytes = val.to_le_bytes();
    storage[offset] = bytes[0];
    storage[offset + 1] = bytes[1];
    storage[offset + 2] = bytes[2];
    storage[offset + 3] = bytes[3];
    storage[offset + 4] = bytes[4];
    storage[offset + 5] = bytes[5];
    storage[offset + 6] = bytes[6];
    storage[offset + 7] = bytes[7];
}

fn clamp_u16(val: u16, min: u16, max: u16) -> u16 {
    if val < min { min } else if val > max { max } else { val }
}
