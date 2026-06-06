use num_complex::Complex32;

#[derive(Clone, Debug)]
pub struct Gsm900Analysis {
    pub in_pgsm_downlink: bool,
    pub arfcn: Option<i32>,
    pub carrier_hz: f64,
    pub carrier_offset_hz: f64,
    pub carrier_visible: bool,
    pub sample_rate_ok: bool,
    pub fcch_freq_hz: f64,
    pub fcch_visible: bool,
    pub fcch_peak_db: Option<f32>,
    pub noise_floor_db: Option<f32>,
    pub fcch_snr_db: Option<f32>,
    pub fcch_detected: bool,
    pub sch_correlation: Option<f32>,
    pub sch_symbol_offset: Option<f32>,
    pub sch_sample_phase: Option<f32>,
    pub sch_detected: bool,
    pub sch_decode: Option<GsmSchDecode>,
    pub last_verified_sch: Option<GsmSchDecode>,
    pub verified_sch_count: u32,
    pub bcch_correlation: Option<f32>,
    pub bcch_detected: bool,
    pub bcch_decode: Option<GsmBcchDecode>,
}

#[derive(Clone, Debug)]
pub struct GsmBcchDecode {
    pub parity_ok: bool,
    pub syndrome: u64,
    pub path_metric: f32,
    pub message_type: Option<u8>,
    pub message_name: &'static str,
    pub l2_hex: String,
    pub c_bits: String,
    pub u_bits: String,
}

#[derive(Clone, Debug)]
pub struct GsmSchDecode {
    pub bsic: u8,
    pub ncc: u8,
    pub bcc: u8,
    pub t1: u16,
    pub t2: u8,
    pub t3p: u8,
    pub frame_number: u32,
    pub path_metric: f32,
    pub parity_ok: bool,
    pub parity_syndrome: u16,
}

impl Default for Gsm900Analysis {
    fn default() -> Self {
        Self {
            in_pgsm_downlink: false,
            arfcn: None,
            carrier_hz: 0.0,
            carrier_offset_hz: 0.0,
            carrier_visible: false,
            sample_rate_ok: false,
            fcch_freq_hz: 0.0,
            fcch_visible: false,
            fcch_peak_db: None,
            noise_floor_db: None,
            fcch_snr_db: None,
            fcch_detected: false,
            sch_correlation: None,
            sch_symbol_offset: None,
            sch_sample_phase: None,
            sch_detected: false,
            sch_decode: None,
            last_verified_sch: None,
            verified_sch_count: 0,
            bcch_correlation: None,
            bcch_detected: false,
            bcch_decode: None,
        }
    }
}

pub const GSM_SYMBOL_RATE_HZ: f64 = 270_833.333;
pub const GSM_FCCH_TONE_HZ: f64 = GSM_SYMBOL_RATE_HZ / 4.0;

pub const SCH_CORRELATION_THRESHOLD: f32 = 0.38;
pub const BCCH_CORRELATION_THRESHOLD: f32 = 0.34;

const NORMAL_TRAINING_BITS: [[i8; 26]; 8] = [
    [0,0,1,0,0,1,0,1,1,1,0,0,0,0,1,0,0,0,1,0,0,1,0,1,1,1],
    [0,0,1,0,1,1,0,1,1,1,0,1,1,1,1,0,0,0,1,0,1,1,0,1,1,1],
    [0,1,0,0,0,0,1,1,1,0,1,1,1,0,1,0,0,1,0,0,0,0,1,1,1,0],
    [0,1,0,0,0,1,1,1,1,0,1,1,0,1,0,0,0,1,0,0,0,1,1,1,1,0],
    [0,0,0,1,1,0,1,0,1,1,1,0,0,1,0,0,0,0,0,1,1,0,1,0,1,1],
    [0,1,0,0,1,1,1,0,1,0,1,1,0,0,0,0,0,1,0,0,1,1,1,0,1,0],
    [1,0,1,0,0,1,1,1,1,1,0,1,1,0,0,0,1,0,1,0,0,1,1,1,1,1],
    [1,1,1,0,1,1,1,1,0,0,0,1,0,0,1,0,1,1,1,0,1,1,1,1,0,0],
];

const SCH_EXTENDED_TRAINING_BITS: [i8; 64] = [
    1, 0, 1, 1, 1, 0, 0, 1,
    0, 1, 1, 0, 0, 0, 1, 0,
    0, 0, 0, 0, 0, 1, 0, 0,
    0, 0, 0, 0, 1, 1, 1, 1,
    0, 0, 1, 0, 1, 1, 0, 1,
    0, 1, 0, 0, 0, 1, 0, 1,
    0, 1, 1, 1, 0, 1, 1, 0,
    0, 0, 0, 1, 1, 0, 1, 1,
];

pub fn analyze_pgsm900_downlink(
    center_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    spectrum_db_fftshifted: &[f32],
    iq: &[Complex32],
) -> Gsm900Analysis {
    let center_mhz = center_hz / 1e6;
    let arfcn = ((center_mhz - 935.0) / 0.2).round() as i32;
    let in_pgsm_downlink = (1..=124).contains(&arfcn);
    let carrier_hz = (935.0 + 0.2 * arfcn as f64) * 1e6;
    let carrier_offset_hz = carrier_hz - center_hz;
    let carrier_visible = in_pgsm_downlink && carrier_offset_hz.abs() + 100_000.0 <= bandwidth_hz / 2.0;
    let sample_rate_ok = sample_rate_hz >= GSM_SYMBOL_RATE_HZ;
    let fcch_freq_hz = carrier_offset_hz + GSM_FCCH_TONE_HZ;
    let fcch_visible = fcch_freq_hz.abs() <= sample_rate_hz / 2.0;

    let mut out = Gsm900Analysis {
        in_pgsm_downlink,
        arfcn: in_pgsm_downlink.then_some(arfcn),
        carrier_hz,
        carrier_offset_hz,
        carrier_visible,
        sample_rate_ok,
        fcch_freq_hz,
        fcch_visible,
        ..Default::default()
    };

    if !(in_pgsm_downlink && carrier_visible && sample_rate_ok && fcch_visible) {
        return out;
    }
    if spectrum_db_fftshifted.len() < 32 || sample_rate_hz <= 0.0 {
        return out;
    }

    let n = spectrum_db_fftshifted.len();
    let Some(fcch_bin) = freq_to_shifted_bin(fcch_freq_hz, sample_rate_hz, n) else {
        return out;
    };

    let hz_per_bin = sample_rate_hz / n as f64;
    let peak_radius = ((6_000.0 / hz_per_bin).ceil() as isize).max(1);
    let noise_radius = ((100_000.0 / hz_per_bin).ceil() as isize).max(peak_radius + 2);
    let exclude_radius = ((15_000.0 / hz_per_bin).ceil() as isize).max(peak_radius + 1);

    let mut peak = f32::NEG_INFINITY;
    for idx in bin_window(fcch_bin, peak_radius, n) {
        peak = peak.max(spectrum_db_fftshifted[idx]);
    }

    let mut noise = Vec::new();
    for idx in bin_window(fcch_bin, noise_radius, n) {
        let dist = circular_distance(idx, fcch_bin, n);
        if dist as isize > exclude_radius {
            noise.push(spectrum_db_fftshifted[idx]);
        }
    }
    if noise.len() < 8 {
        return out;
    }
    noise.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_floor = noise[noise.len() / 2];
    let snr = peak - noise_floor;

    out.fcch_peak_db = Some(peak);
    out.noise_floor_db = Some(noise_floor);
    out.fcch_snr_db = Some(snr);
    // This is intentionally only a coarse FCCH indicator. A real lock still
    // needs burst timing, SCH decode, and frame-number validation.
    out.fcch_detected = snr >= 8.0;

    estimate_sch_sync_candidate(&mut out, center_hz, sample_rate_hz, iq);
    out
}

fn estimate_sch_sync_candidate(
    out: &mut Gsm900Analysis,
    center_hz: f64,
    sample_rate_hz: f64,
    iq: &[Complex32],
) {
    if !out.carrier_visible || !out.sample_rate_ok || iq.len() < 256 || sample_rate_hz <= 0.0 {
        return;
    }

    let samples_per_symbol = sample_rate_hz / GSM_SYMBOL_RATE_HZ;
    if samples_per_symbol < 1.0 {
        return;
    }

    // Mix the nearest ARFCN carrier to baseband, sample several symbol phases,
    // and correlate against the complex GMSK-mapped SCH extended training
    // sequence (TS 05.02 section 5.2.5, BN42..BN105). This is much closer to
    // gr-gsm/osmo-trx style SCH acquisition than the old sign-only detector.
    let carrier_offset_hz = out.carrier_hz - center_hz;
    let carrier_step = -std::f32::consts::TAU * (carrier_offset_hz / sample_rate_hz) as f32;
    let max_phase_trials = samples_per_symbol.ceil().clamp(1.0, 10.0) as usize;
    let sch_seq = gmsk_training_sequence();

    let mut best_corr = 0.0_f32;
    let mut best_symbol_offset = None;
    let mut best_sample_phase = None;
    let mut candidates: Vec<SchCandidate> = Vec::new();

    for trial in 0..max_phase_trials {
        let phase_samples = trial as f64 * samples_per_symbol / max_phase_trials as f64;
        let symbol_count = ((iq.len() as f64 - phase_samples) / samples_per_symbol).floor() as usize;
        if symbol_count <= 149 {
            continue;
        }

        let mut symbols = Vec::with_capacity(symbol_count);
        for sym in 0..symbol_count {
            symbols.push(corrected_sample(iq, sym, phase_samples, samples_per_symbol, carrier_step));
        }

        for train_start in 0..=symbols.len().saturating_sub(sch_seq.len()) {
            let score = complex_sequence_correlation(&symbols[train_start..train_start + sch_seq.len()], &sch_seq);
            let burst_start = train_start as isize - 42;
            if score > best_corr {
                best_corr = score;
                best_symbol_offset = Some(burst_start as f32);
                best_sample_phase = Some(phase_samples as f32);
            }
            if score >= SCH_CORRELATION_THRESHOLD * 0.72
                && burst_start >= 0
                && (burst_start as usize + 148) < symbols.len()
            {
                let soft = soft_slice_sch_burst_bits(
                    &symbols[burst_start as usize..burst_start as usize + 148],
                );
                push_sch_candidate(&mut candidates, SchCandidate {
                    score,
                    symbol_offset: burst_start as f32,
                    sample_phase: phase_samples as f32,
                    soft,
                });
            }
        }
    }

    if best_corr > 0.0 {
        out.sch_correlation = Some(best_corr);
        out.sch_symbol_offset = best_symbol_offset;
        out.sch_sample_phase = best_sample_phase;
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        write_sch_debug_dump(center_hz, sample_rate_hz, out.carrier_hz, best_corr, &candidates);
        out.sch_detected = best_corr >= SCH_CORRELATION_THRESHOLD;
        if out.sch_detected {
            let mut best_decode = None;
            for candidate in &candidates {
                let Some(decode) = decode_sch_candidate(&candidate.soft) else {
                    continue;
                };
                if decode.parity_ok {
                    out.sch_correlation = Some(candidate.score);
                    out.sch_symbol_offset = Some(candidate.symbol_offset);
                    out.sch_sample_phase = Some(candidate.sample_phase);
                    out.sch_decode = Some(decode);
                    return;
                }
                let replace = best_decode
                    .as_ref()
                    .map(|old: &GsmSchDecode| {
                        decode.parity_syndrome.count_ones() < old.parity_syndrome.count_ones()
                            || (decode.parity_syndrome.count_ones() == old.parity_syndrome.count_ones()
                                && decode.path_metric < old.path_metric)
                    })
                    .unwrap_or(true);
                if replace {
                    best_decode = Some(decode);
                }
            }
            out.sch_decode = best_decode;
        }
    }
}

struct SchCandidate {
    score: f32,
    symbol_offset: f32,
    sample_phase: f32,
    soft: Vec<f32>,
}

fn write_sch_debug_dump(
    center_hz: f64,
    sample_rate_hz: f64,
    carrier_hz: f64,
    best_corr: f32,
    candidates: &[SchCandidate],
) {
    let paths = [
        "/sdcard/Android/data/org.pixdr.app/files/pixdr_gsm_sch_dump.txt",
        "/data/data/org.pixdr.app/files/pixdr_gsm_sch_dump.txt",
    ];
    let mut content = format!(
        "pixdr GSM SCH dump\ncenter_hz={center_hz:.3}\nsample_rate_hz={sample_rate_hz:.3}\ncarrier_hz={carrier_hz:.3}\nbest_corr={best_corr:.6}\ncandidates={}\n",
        candidates.len()
    );
    for (idx, candidate) in candidates.iter().take(8).enumerate() {
        content.push_str(&format!(
            "\n# candidate {idx} score={:.6} symbol_offset={:.3} sample_phase={:.6}\n",
            candidate.score, candidate.symbol_offset, candidate.sample_phase
        ));
        if candidate.soft.len() >= 145 {
            let mut base = [0.0_f32; 78];
            base[..39].copy_from_slice(&candidate.soft[3..42]);
            base[39..].copy_from_slice(&candidate.soft[106..145]);
            content.push_str(&format!("c_soft_hard={}\n", soft_bits_to_string(&base)));
            for invert_coded in [false, true] {
                for swap_pair_order in [false, true] {
                    let mut coded = base;
                    if invert_coded {
                        for bit in &mut coded {
                            *bit = -*bit;
                        }
                    }
                    if swap_pair_order {
                        for pair in coded.chunks_exact_mut(2) {
                            pair.swap(0, 1);
                        }
                    }
                    let Some((uncoded, metric)) = soft_viterbi_decode_gsm_k5_len::<39>(&coded) else {
                        continue;
                    };
                    for parity_reversed in [false, true] {
                        for parity_inverted in [false, true] {
                            let decode = build_sch_decode(&uncoded, metric, parity_reversed, parity_inverted);
                            content.push_str(&format!(
                                "variant inv={invert_coded} swap={swap_pair_order} prev={parity_reversed} pinv={parity_inverted} ok={} syn=0x{:03x} pop={} metric={:.6} bsic={} ncc={} bcc={} fn={} fn_mod51={} u={}\n",
                                decode.parity_ok,
                                decode.parity_syndrome,
                                decode.parity_syndrome.count_ones(),
                                decode.path_metric,
                                decode.bsic,
                                decode.ncc,
                                decode.bcc,
                                decode.frame_number,
                                decode.frame_number % 51,
                                bool_bits_to_string(&uncoded),
                            ));
                        }
                    }
                }
            }
        }
    }
    for path in paths {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, &content);
    }
}

fn push_sch_candidate(candidates: &mut Vec<SchCandidate>, candidate: SchCandidate) {
    const MAX_SCH_CANDIDATES: usize = 18;
    const NON_MAX_SUPPRESSION_SYMBOLS: f32 = 6.0;

    if let Some(existing) = candidates
        .iter_mut()
        .find(|old| (old.symbol_offset - candidate.symbol_offset).abs() <= NON_MAX_SUPPRESSION_SYMBOLS)
    {
        if candidate.score > existing.score {
            *existing = candidate;
        }
        return;
    }

    if candidates.len() < MAX_SCH_CANDIDATES {
        candidates.push(candidate);
        return;
    }

    if let Some((idx, weakest)) = candidates
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
    {
        if candidate.score > weakest.score {
            candidates[idx] = candidate;
        }
    }
}

pub fn decode_bcch_from_sch_timing(
    center_hz: f64,
    sample_rate_hz: f64,
    carrier_hz: f64,
    bcc: u8,
    iq: &[Complex32],
    sch_symbol_offset: Option<f32>,
    sch_sample_phase: Option<f32>,
    decode_blocks: bool,
) -> (Option<f32>, bool, Option<GsmBcchDecode>) {
    let (Some(sch_symbol_offset), Some(sch_sample_phase)) = (sch_symbol_offset, sch_sample_phase) else {
        return estimate_bcch_normal_burst(center_hz, sample_rate_hz, carrier_hz, bcc, iq, false);
    };
    if iq.len() < 256 || sample_rate_hz <= 0.0 || bcc > 7 {
        return (None, false, None);
    }
    let samples_per_symbol = sample_rate_hz / GSM_SYMBOL_RATE_HZ;
    if samples_per_symbol < 1.0 {
        return (None, false, None);
    }

    let carrier_offset_hz = carrier_hz - center_hz;
    let carrier_step = -std::f32::consts::TAU * (carrier_offset_hz / sample_rate_hz) as f32;
    let symbol_count = ((iq.len() as f64 - sch_sample_phase as f64) / samples_per_symbol).floor() as usize;
    if symbol_count <= 149 {
        return (None, false, None);
    }
    let mut symbols = Vec::with_capacity(symbol_count);
    for sym in 0..symbol_count {
        symbols.push(corrected_sample(
            iq,
            sym,
            sch_sample_phase as f64,
            samples_per_symbol,
            carrier_step,
        ));
    }

    let normal_seq = normal_training_sequence(bcc as usize);
    let sch_start = sch_symbol_offset.round() as isize;
    let mut best_corr = 0.0_f32;
    let mut candidates: Vec<BcchBurstCandidate> = Vec::new();

    // TS0 bursts repeat every GSM frame = 8 * 156.25 = 1250 symbols.
    // TS0 blocks around the SCH burst may be before or after the detected SCH
    // within the rolling buffer. Try scheduled frame offsets instead of scanning
    // the whole buffer.
    const FRAME: isize = 1250;
    const FRAME_DELTAS: [isize; 28] = [
        -41, -31, -25, -21, -15, -11, -10, -9, -8, -7, -6, -5, -4, -3,
        -2, -1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 15,
    ];
    for delta in FRAME_DELTAS {
        let base = sch_start + delta * FRAME;
        for jitter in [-2isize, -1, 0, 1, 2] {
            let burst0 = base + jitter;
            if burst0 < 0 {
                continue;
            }
            let burst0 = burst0 as usize;
            if burst0 + 3 * 1250 + 148 > symbols.len() {
                continue;
            }
            let train = burst0 + 61;
            if train + normal_seq.len() > symbols.len() {
                continue;
            }
            let corr = complex_sequence_correlation(&symbols[train..train + normal_seq.len()], &normal_seq);
            best_corr = best_corr.max(corr);
            if corr >= BCCH_CORRELATION_THRESHOLD * 0.80 {
                for bit_shift in [-1isize, 0, 1] {
                    if let Some(i_soft) = extract_xcch_i_soft(&symbols, burst0, bcc, bit_shift) {
                        push_bcch_candidate(&mut candidates, BcchBurstCandidate {
                            score: corr,
                            burst_start: burst0,
                            bit_shift,
                            i_soft,
                        });
                    }
                }
            }
        }
    }

    if !decode_blocks {
        return (Some(best_corr), best_corr >= BCCH_CORRELATION_THRESHOLD, None);
    }

    let mut best_decode = None;
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    for candidate in &candidates {
        let Some(decode) = decode_bcch_candidate(candidate) else {
            continue;
        };
        if decode.parity_ok {
            return (Some(best_corr), best_corr >= BCCH_CORRELATION_THRESHOLD, Some(decode));
        }
        let replace = best_decode
            .as_ref()
            .map(|old: &GsmBcchDecode| {
                decode.syndrome.count_ones() < old.syndrome.count_ones()
                    || (decode.syndrome.count_ones() == old.syndrome.count_ones()
                        && decode.path_metric < old.path_metric)
            })
            .unwrap_or(true);
        if replace {
            best_decode = Some(decode);
        }
    }

    (Some(best_corr), best_corr >= BCCH_CORRELATION_THRESHOLD, best_decode)
}

pub fn estimate_bcch_normal_burst(
    center_hz: f64,
    sample_rate_hz: f64,
    carrier_hz: f64,
    bcc: u8,
    iq: &[Complex32],
    decode_blocks: bool,
) -> (Option<f32>, bool, Option<GsmBcchDecode>) {
    if iq.len() < 256 || sample_rate_hz <= 0.0 || bcc > 7 {
        return (None, false, None);
    }
    let samples_per_symbol = sample_rate_hz / GSM_SYMBOL_RATE_HZ;
    if samples_per_symbol < 1.0 {
        return (None, false, None);
    }

    let carrier_offset_hz = carrier_hz - center_hz;
    let carrier_step = -std::f32::consts::TAU * (carrier_offset_hz / sample_rate_hz) as f32;
    let max_phase_trials = samples_per_symbol.ceil().clamp(1.0, 10.0) as usize;
    let seq = normal_training_sequence(bcc as usize);
    let mut best = 0.0_f32;
    let mut decode_candidates: Vec<BcchBurstCandidate> = Vec::new();

    for trial in 0..max_phase_trials {
        let phase_samples = trial as f64 * samples_per_symbol / max_phase_trials as f64;
        let symbol_count = ((iq.len() as f64 - phase_samples) / samples_per_symbol).floor() as usize;
        if symbol_count <= 4 * 1250 + 148 {
            continue;
        }
        let mut symbols = Vec::with_capacity(symbol_count);
        for sym in 0..symbol_count {
            symbols.push(corrected_sample(iq, sym, phase_samples, samples_per_symbol, carrier_step));
        }
        for train_start in 0..=symbols.len().saturating_sub(seq.len()) {
            let score = complex_sequence_correlation(&symbols[train_start..train_start + seq.len()], &seq);
            best = best.max(score);
            let burst_start = train_start as isize - 61;
            if decode_blocks && score >= BCCH_CORRELATION_THRESHOLD * 0.86 && burst_start >= 0 {
                // The detected normal burst may be burst 0, 1, 2, or 3 of an
                // XCCH 4-burst block. Try all four block alignments, plus a
                // tiny symbol timing jitter around the midamble-derived start.
                for block_pos in 0..4usize {
                    let Some(base) = (burst_start as usize).checked_sub(block_pos * 1250) else {
                        continue;
                    };
                    for jitter in [-2isize, -1, 0, 1, 2] {
                        let Some(aligned_base) = apply_symbol_jitter(base, jitter) else {
                            continue;
                        };
                        for bit_shift in [-1isize, 0, 1] {
                            if let Some(i_soft) = extract_xcch_i_soft(&symbols, aligned_base, bcc, bit_shift) {
                                push_bcch_candidate(&mut decode_candidates, BcchBurstCandidate {
                                    score,
                                    burst_start: aligned_base,
                                    bit_shift,
                                    i_soft,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    let mut best_decode = None;
    decode_candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    for candidate in &decode_candidates {
        let Some(decode) = decode_bcch_candidate(candidate) else {
            continue;
        };
        if decode.parity_ok {
            return (Some(best), best >= BCCH_CORRELATION_THRESHOLD, Some(decode));
        }
        let replace = best_decode
            .as_ref()
            .map(|old: &GsmBcchDecode| {
                decode.syndrome.count_ones() < old.syndrome.count_ones()
                    || (decode.syndrome.count_ones() == old.syndrome.count_ones()
                        && decode.path_metric < old.path_metric)
            })
            .unwrap_or(true);
        if replace {
            best_decode = Some(decode);
        }
    }

    (Some(best), best >= BCCH_CORRELATION_THRESHOLD, best_decode)
}

struct BcchBurstCandidate {
    score: f32,
    burst_start: usize,
    bit_shift: isize,
    i_soft: [f32; 456],
}

fn push_bcch_candidate(candidates: &mut Vec<BcchBurstCandidate>, candidate: BcchBurstCandidate) {
    const MAX_BCCH_CANDIDATES: usize = 48;
    const NON_MAX_SUPPRESSION_SYMBOLS: usize = 16;
    if let Some(existing) = candidates
        .iter_mut()
        .find(|old| old.bit_shift == candidate.bit_shift
            && old.burst_start.abs_diff(candidate.burst_start) <= NON_MAX_SUPPRESSION_SYMBOLS)
    {
        if candidate.score > existing.score {
            *existing = candidate;
        }
        return;
    }
    if candidates.len() < MAX_BCCH_CANDIDATES {
        candidates.push(candidate);
        return;
    }
    if let Some((idx, weakest)) = candidates
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
    {
        if candidate.score > weakest.score {
            candidates[idx] = candidate;
        }
    }
}

fn apply_symbol_jitter(base: usize, jitter: isize) -> Option<usize> {
    if jitter < 0 {
        base.checked_sub(jitter.unsigned_abs())
    } else {
        base.checked_add(jitter as usize)
    }
}

fn extract_xcch_i_soft(
    symbols: &[Complex32],
    burst0: usize,
    bcc: u8,
    bit_shift: isize,
) -> Option<[f32; 456]> {
    let mut i_soft = [0.0_f32; 456];
    let tsc = usize::from(bcc & 0x07);
    let normal_seq = normal_training_sequence(tsc);
    for b in 0..4 {
        let expected = burst0 + b * 1250;
        let start = refine_normal_burst_start(symbols, expected, &normal_seq)?;
        if start + 148 > symbols.len() {
            return None;
        }
        let burst = soft_slice_normal_burst_bits(&symbols[start..start + 148], bcc);
        if burst.len() < 145 {
            return None;
        }
        let dst = b * 114;
        copy_shifted_soft(&burst, 3, &mut i_soft[dst..dst + 57], bit_shift)?;
        copy_shifted_soft(&burst, 88, &mut i_soft[dst + 57..dst + 114], bit_shift)?;
    }
    Some(i_soft)
}

fn refine_normal_burst_start(
    symbols: &[Complex32],
    expected_start: usize,
    normal_seq: &[Complex32; 26],
) -> Option<usize> {
    let mut best = None;
    let mut best_score = 0.0_f32;
    for jitter in -8isize..=8 {
        let start = apply_symbol_jitter(expected_start, jitter)?;
        let train = start + 61;
        if train + normal_seq.len() > symbols.len() || start + 148 > symbols.len() {
            continue;
        }
        let score = complex_sequence_correlation(&symbols[train..train + normal_seq.len()], normal_seq);
        if score > best_score {
            best_score = score;
            best = Some(start);
        }
    }
    best
}

fn copy_shifted_soft(src: &[f32], start: usize, dst: &mut [f32], shift: isize) -> Option<()> {
    for (i, out) in dst.iter_mut().enumerate() {
        let idx = start as isize + i as isize + shift;
        if idx < 0 || idx as usize >= src.len() {
            return None;
        }
        *out = src[idx as usize];
    }
    Some(())
}

fn decode_bcch_candidate(candidate: &BcchBurstCandidate) -> Option<GsmBcchDecode> {
    let mut best = None;
    for b_rotation in 0..4 {
        let i_soft = rotate_xcch_i_soft(&candidate.i_soft, b_rotation);
        let coded = xcch_deinterleave_soft(&i_soft);
        for invert_coded in [false, true] {
            for swap_pair_order in [false, true] {
                let mut c = coded;
                if invert_coded {
                    for bit in &mut c {
                        *bit = -*bit;
                    }
                }
                if swap_pair_order {
                    for pair in c.chunks_exact_mut(2) {
                        pair.swap(0, 1);
                    }
                }
                let Some((u, metric)) = soft_viterbi_decode_gsm_k5_len::<228>(&c) else {
                    continue;
                };
                for parity_inverted in [true, false] {
                    for parity_reversed in [false, true] {
                        for expected_remainder in [0, (1u64 << 40) - 1] {
                            let decode = build_xcch_decode(
                                &c,
                                &u,
                                metric,
                                parity_inverted,
                                parity_reversed,
                                expected_remainder,
                            );
                            if decode.parity_ok {
                                return Some(decode);
                            }
                            let replace = best
                                .as_ref()
                                .map(|old: &GsmBcchDecode| {
                                    decode.syndrome.count_ones() < old.syndrome.count_ones()
                                        || (decode.syndrome.count_ones() == old.syndrome.count_ones()
                                            && decode.path_metric < old.path_metric)
                                })
                                .unwrap_or(true);
                            if replace {
                                best = Some(decode);
                            }
                        }
                    }
                }
            }
        }
    }
    best
}

fn rotate_xcch_i_soft(i_soft: &[f32; 456], rotation: usize) -> [f32; 456] {
    let mut out = [0.0_f32; 456];
    for b in 0..4 {
        let src_b = (b + rotation) % 4;
        out[b * 114..b * 114 + 114]
            .copy_from_slice(&i_soft[src_b * 114..src_b * 114 + 114]);
    }
    out
}

fn xcch_deinterleave_soft(i_soft: &[f32; 456]) -> [f32; 456] {
    let mut c = [0.0_f32; 456];
    for k in 0..456 {
        let b = k % 4;
        let j = 2 * ((49 * k) % 57) + ((k % 8) / 4);
        c[k] = i_soft[b * 114 + j];
    }
    c
}

fn build_xcch_decode(
    c_soft: &[f32; 456],
    u: &[bool; 228],
    metric: f32,
    parity_inverted: bool,
    parity_reversed: bool,
    expected_remainder: u64,
) -> GsmBcchDecode {
    let mut data = [false; 184];
    data.copy_from_slice(&u[..184]);
    let mut parity = [false; 40];
    parity.copy_from_slice(&u[184..224]);
    if parity_reversed {
        parity.reverse();
    }
    if parity_inverted {
        for bit in &mut parity {
            *bit = !*bit;
        }
    }
    let syndrome = block_parity_syndrome(
        data.iter().copied().chain(parity.iter().copied()),
        0x10004820009,
        40,
    );
    let l2 = pack_lsb8msb_bytes(&data);
    let message_type = l2.get(2).copied();
    GsmBcchDecode {
        parity_ok: syndrome == expected_remainder,
        syndrome,
        path_metric: metric,
        message_type,
        message_name: message_type.map(gsm_rr_message_name).unwrap_or("unknown"),
        l2_hex: l2.iter().take(8).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
        c_bits: soft_bits_to_string(c_soft),
        u_bits: bool_bits_to_string(u),
    }
}

fn block_parity_syndrome<I>(bits: I, poly: u64, degree: u32) -> u64
where
    I: IntoIterator<Item = bool>,
{
    let top = 1u64 << degree;
    let mask = (top << 1) - 1;
    let mut rem = 0u64;
    for bit in bits {
        rem = ((rem << 1) | u64::from(bit)) & mask;
        if (rem & top) != 0 {
            rem ^= poly;
        }
    }
    rem & (top - 1)
}

fn soft_bits_to_string(bits: &[f32]) -> String {
    bits.iter().map(|b| if *b >= 0.0 { '1' } else { '0' }).collect()
}

fn bool_bits_to_string(bits: &[bool]) -> String {
    bits.iter().map(|b| if *b { '1' } else { '0' }).collect()
}

fn pack_lsb8msb_bytes(bits: &[bool; 184]) -> Vec<u8> {
    let mut ordered = *bits;
    lsb8msb(&mut ordered);
    ordered
        .chunks(8)
        .map(|chunk| chunk.iter().fold(0u8, |acc, bit| (acc << 1) | u8::from(*bit)))
        .collect()
}

fn gsm_rr_message_name(message_type: u8) -> &'static str {
    match message_type & 0x3f {
        0x19 => "System Information 1",
        0x1a => "System Information 2",
        0x1b => "System Information 3",
        0x1c => "System Information 4",
        0x1d => "System Information 5",
        0x1e => "System Information 6",
        0x1f => "System Information 7/8",
        0x2d => "System Information 13",
        0x21 => "Paging Request 1",
        0x22 => "Paging Request 2",
        0x24 => "Paging Request 3",
        0x3f => "Immediate Assignment",
        _ => "GSM RR/LAPDm",
    }
}

fn normal_training_sequence(tsc: usize) -> [Complex32; 26] {
    let mut out = [Complex32::new(0.0, 0.0); 26];
    let bits = &NORMAL_TRAINING_BITS[tsc];
    let j = Complex32::new(0.0, 1.0);
    out[0] = if bits[0] == 0 {
        Complex32::new(1.0, 0.0)
    } else {
        Complex32::new(-1.0, 0.0)
    };
    let mut prev = 2 * bits[0] - 1;
    for i in 1..bits.len() {
        let cur = 2 * bits[i] - 1;
        let enc = cur * prev;
        out[i] = j * Complex32::new(enc as f32, 0.0) * out[i - 1];
        prev = cur;
    }
    out
}

fn gmsk_training_sequence() -> [Complex32; 64] {
    let mut out = [Complex32::new(0.0, 0.0); 64];
    let j = Complex32::new(0.0, 1.0);
    let start = if SCH_EXTENDED_TRAINING_BITS[0] == 0 {
        Complex32::new(1.0, 0.0)
    } else {
        Complex32::new(-1.0, 0.0)
    };
    out[0] = start;
    let mut prev = 2 * SCH_EXTENDED_TRAINING_BITS[0] - 1;
    for i in 1..SCH_EXTENDED_TRAINING_BITS.len() {
        let cur = 2 * SCH_EXTENDED_TRAINING_BITS[i] - 1;
        let enc = cur * prev;
        out[i] = j * Complex32::new(enc as f32, 0.0) * out[i - 1];
        prev = cur;
    }
    out
}

fn complex_sequence_correlation(input: &[Complex32], seq: &[Complex32]) -> f32 {
    let mut corr = Complex32::new(0.0, 0.0);
    let mut input_energy = 0.0_f32;
    let mut seq_energy = 0.0_f32;
    for (x, s) in input.iter().zip(seq.iter()) {
        corr += *s * x.conj();
        input_energy += x.norm_sqr();
        seq_energy += s.norm_sqr();
    }
    let denom = (input_energy * seq_energy).sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        (corr.norm() / denom).min(1.0)
    }
}

fn hard_slice_sch_burst_bits(symbols: &[Complex32]) -> Vec<bool> {
    soft_slice_sch_burst_bits(symbols)
        .into_iter()
        .map(|s| s >= 0.0)
        .collect()
}

fn soft_slice_sch_burst_bits(symbols: &[Complex32]) -> Vec<f32> {
    soft_slice_burst_bits_with_training(symbols, 42, &SCH_EXTENDED_TRAINING_BITS)
}

fn hard_slice_normal_burst_bits(symbols: &[Complex32], bcc: u8) -> Vec<bool> {
    soft_slice_normal_burst_bits(symbols, bcc)
        .into_iter()
        .map(|s| s >= 0.0)
        .collect()
}

fn soft_slice_normal_burst_bits(symbols: &[Complex32], bcc: u8) -> Vec<f32> {
    let tsc = usize::from(bcc & 0x07);
    soft_slice_burst_bits_with_training(symbols, 61, &NORMAL_TRAINING_BITS[tsc])
}

fn soft_slice_burst_bits_with_training(
    symbols: &[Complex32],
    training_offset: usize,
    training_bits: &[i8],
) -> Vec<f32> {
    if symbols.len() < 2 {
        return Vec::new();
    }

    let mut phases = Vec::with_capacity(symbols.len());
    phases.push(0.0_f32);
    for pair in symbols.windows(2) {
        let diff = pair[1] * pair[0].conj();
        phases.push(diff.im.atan2(diff.re));
    }

    // Estimate residual phase/frequency bias from the known midamble. Without
    // this, even a small CFO shifts all differential phases and corrupts the
    // 4-burst xCCH block. Try both polarities and choose the lower residual.
    let mut best_bias = 0.0_f32;
    let mut best_polarity = 1.0_f32;
    let mut best_err = f32::INFINITY;
    if phases.len() >= training_offset + training_bits.len() {
        for polarity in [1.0_f32, -1.0] {
            let mut sin_sum = 0.0_f32;
            let mut cos_sum = 0.0_f32;
            for (i, bit) in training_bits.iter().enumerate() {
                let expected = polarity * if *bit != 0 { 1.0 } else { -1.0 };
                let err = wrap_phase(phases[training_offset + i] - expected * std::f32::consts::FRAC_PI_2);
                sin_sum += err.sin();
                cos_sum += err.cos();
            }
            let bias = sin_sum.atan2(cos_sum);
            let mut err_sum = 0.0_f32;
            for (i, bit) in training_bits.iter().enumerate() {
                let expected = polarity * if *bit != 0 { 1.0 } else { -1.0 };
                let err = wrap_phase(phases[training_offset + i] - bias - expected * std::f32::consts::FRAC_PI_2);
                err_sum += err.abs();
            }
            if err_sum < best_err {
                best_err = err_sum;
                best_bias = bias;
                best_polarity = polarity;
            }
        }
    }

    phases
        .into_iter()
        .map(|phase| {
            let corrected = wrap_phase(phase - best_bias) / std::f32::consts::FRAC_PI_2;
            (best_polarity * corrected).clamp(-1.0, 1.0)
        })
        .collect()
}

fn wrap_phase(mut x: f32) -> f32 {
    while x > std::f32::consts::PI {
        x -= std::f32::consts::TAU;
    }
    while x < -std::f32::consts::PI {
        x += std::f32::consts::TAU;
    }
    x
}

fn hard_slice_burst_bits_with_training(
    symbols: &[Complex32],
    training_offset: usize,
    training_bits: &[i8],
) -> Vec<bool> {
    if symbols.len() < 2 {
        return Vec::new();
    }
    let mut bits = Vec::with_capacity(symbols.len());
    bits.push(false);
    for pair in symbols.windows(2) {
        let diff = pair[1] * pair[0].conj();
        bits.push(diff.im.atan2(diff.re) >= 0.0);
    }

    if bits.len() >= training_offset + training_bits.len() {
        let mut matches = 0usize;
        for (i, expected) in training_bits.iter().enumerate() {
            if bits[training_offset + i] == (*expected != 0) {
                matches += 1;
            }
        }
        if matches < training_bits.len() / 2 {
            for bit in &mut bits {
                *bit = !*bit;
            }
        }
    }
    bits
}

fn decode_sch_candidate(burst_soft: &[f32]) -> Option<GsmSchDecode> {
    if burst_soft.len() < 145 {
        return None;
    }
    let mut base = [0.0_f32; 78];
    base[..39].copy_from_slice(&burst_soft[3..42]);
    base[39..].copy_from_slice(&burst_soft[106..145]);

    let mut best: Option<GsmSchDecode> = None;
    for invert_coded in [false, true] {
        for swap_pair_order in [false, true] {
            let mut coded = base;
            if invert_coded {
                for bit in &mut coded {
                    *bit = -*bit;
                }
            }
            if swap_pair_order {
                for pair in coded.chunks_exact_mut(2) {
                    pair.swap(0, 1);
                }
            }
            let Some((uncoded, metric)) = soft_viterbi_decode_gsm_k5_len::<39>(&coded) else {
                continue;
            };
            for parity_reversed in [false, true] {
                for parity_inverted in [false, true] {
                    let candidate = build_sch_decode(&uncoded, metric, parity_reversed, parity_inverted);
                    let replace = best
                        .as_ref()
                        .map(|old| {
                            candidate.parity_syndrome.count_ones() < old.parity_syndrome.count_ones()
                                || (candidate.parity_syndrome.count_ones() == old.parity_syndrome.count_ones()
                                    && candidate.path_metric < old.path_metric)
                        })
                        .unwrap_or(true);
                    if replace {
                        best = Some(candidate);
                    }
                }
            }
        }
    }
    best
}

fn build_sch_decode(
    uncoded: &[bool; 39],
    metric: f32,
    parity_reversed: bool,
    parity_inverted: bool,
) -> GsmSchDecode {
    let mut data = [false; 25];
    data.copy_from_slice(&uncoded[..25]);
    let mut parity = [false; 10];
    parity.copy_from_slice(&uncoded[25..35]);
    if parity_reversed {
        parity.reverse();
    }
    if parity_inverted {
        for bit in &mut parity {
            *bit = !*bit;
        }
    }
    let syndrome = sch_parity_syndrome(&data, &parity);
    let parsed = parse_sch_info_bits(&data);
    GsmSchDecode {
        bsic: parsed.0,
        ncc: parsed.0 >> 3,
        bcc: parsed.0 & 0x07,
        t1: parsed.1,
        t2: parsed.2,
        t3p: parsed.3,
        frame_number: sch_frame_number(parsed.1, parsed.2, parsed.3),
        path_metric: metric,
        parity_ok: syndrome == 0,
        parity_syndrome: syndrome,
    }
}

fn sch_parity_syndrome(data: &[bool; 25], parity: &[bool; 10]) -> u16 {
    // SCH uses the GSM 05.03 10-bit parity polynomial used by OpenBTS as
    // BlockCoder(0x0575, 10, 25). The Viterbi output is still in channel
    // coding bit order: 25 protected data bits followed by 10 parity bits and
    // four zero convolutional tail bits. A valid SCH candidate should produce
    // zero remainder when the 35-bit d+p word is divided by the degree-10
    // generator polynomial.
    let mut rem = 0u16;
    for bit in data.iter().chain(parity.iter()).copied() {
        rem = ((rem << 1) | u16::from(bit)) & 0x07ff;
        if (rem & 0x0400) != 0 {
            rem ^= 0x0575;
        }
    }
    rem & 0x03ff
}

fn viterbi_decode_gsm_k5(coded: &[bool; 78]) -> Option<([bool; 39], f32)> {
    viterbi_decode_gsm_k5_len::<39>(coded)
}

fn viterbi_decode_gsm_k5_len<const OUT: usize>(coded: &[bool]) -> Option<([bool; OUT], f32)> {
    const STATES: usize = 16;
    const INF: i32 = 1_000_000;
    if coded.len() < OUT * 2 {
        return None;
    }
    let mut metrics = [INF; STATES];
    metrics[0] = 0;
    let mut prev = vec![[(0usize, false); STATES]; OUT];

    for step in 0..OUT {
        let c0 = coded[2 * step];
        let c1 = coded[2 * step + 1];
        let mut next_metrics = [INF; STATES];
        for (state, &metric) in metrics.iter().enumerate() {
            if metric >= INF {
                continue;
            }
            for bit in [false, true] {
                let next = ((state << 1) & 0x0e) | usize::from(bit);
                let (e0, e1) = gsm_conv_outputs(state, bit);
                let branch = i32::from(e0 != c0) + i32::from(e1 != c1);
                let candidate = metric + branch;
                if candidate < next_metrics[next] {
                    next_metrics[next] = candidate;
                    prev[step][next] = (state, bit);
                }
            }
        }
        metrics = next_metrics;
    }

    let mut state = if metrics[0] < INF {
        0
    } else {
        metrics
            .iter()
            .enumerate()
            .min_by_key(|(_, metric)| *metric)
            .map(|(state, _)| state)?
    };
    let final_metric = metrics[state];
    if final_metric >= INF {
        return None;
    }

    let mut bits = [false; OUT];
    for step in (0..OUT).rev() {
        let (ps, bit) = prev[step][state];
        bits[step] = bit;
        state = ps;
    }
    Some((bits, final_metric as f32 / (OUT * 2) as f32))
}

fn soft_viterbi_decode_gsm_k5_len<const OUT: usize>(coded: &[f32]) -> Option<([bool; OUT], f32)> {
    const STATES: usize = 16;
    const INF: f32 = 1.0e30;
    if coded.len() < OUT * 2 {
        return None;
    }
    let mut metrics = [INF; STATES];
    metrics[0] = 0.0;
    let mut prev = vec![[(0usize, false); STATES]; OUT];

    for step in 0..OUT {
        let s0 = coded[2 * step].clamp(-1.0, 1.0);
        let s1 = coded[2 * step + 1].clamp(-1.0, 1.0);
        let mut next_metrics = [INF; STATES];
        for (state, &metric) in metrics.iter().enumerate() {
            if metric >= INF {
                continue;
            }
            for bit in [false, true] {
                let next = ((state << 1) & 0x0e) | usize::from(bit);
                let (e0, e1) = gsm_conv_outputs(state, bit);
                let branch = soft_bit_cost(s0, e0) + soft_bit_cost(s1, e1);
                let candidate = metric + branch;
                if candidate < next_metrics[next] {
                    next_metrics[next] = candidate;
                    prev[step][next] = (state, bit);
                }
            }
        }
        metrics = next_metrics;
    }

    let mut state = if metrics[0].is_finite() && metrics[0] < INF {
        0
    } else {
        metrics
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(state, _)| state)?
    };
    let final_metric = metrics[state];
    if !final_metric.is_finite() || final_metric >= INF {
        return None;
    }
    let mut bits = [false; OUT];
    for step in (0..OUT).rev() {
        let (ps, bit) = prev[step][state];
        bits[step] = bit;
        state = ps;
    }
    Some((bits, final_metric / (OUT * 2) as f32))
}

fn soft_bit_cost(sample: f32, expected: bool) -> f32 {
    // sample > 0 means hard decision '1', sample < 0 means '0'.
    // Lower cost for matching sign, with magnitude as confidence.
    let expected_sign = if expected { 1.0 } else { -1.0 };
    1.0 - sample * expected_sign
}

fn gsm_conv_outputs(state: usize, bit: bool) -> (bool, bool) {
    let b = usize::from(bit);
    let d1 = state & 1;
    let d3 = (state >> 2) & 1;
    let d4 = (state >> 3) & 1;
    // GSM 05.03 K=5 polynomials used by SCH: 0x19 and 0x1b
    // interpreted as current + D3 + D4 and current + D1 + D3 + D4.
    ((b ^ d3 ^ d4) != 0, (b ^ d1 ^ d3 ^ d4) != 0)
}

fn parse_sch_info_bits(encoded_order: &[bool; 25]) -> (u8, u16, u8, u8) {
    let mut bits = *encoded_order;
    lsb8msb(&mut bits);
    let mut offset = 0usize;
    let bsic = read_bits(&bits, &mut offset, 6) as u8;
    let t1 = read_bits(&bits, &mut offset, 11) as u16;
    let t2 = read_bits(&bits, &mut offset, 5) as u8;
    let t3p = read_bits(&bits, &mut offset, 3) as u8;
    (bsic, t1, t2, t3p)
}

fn lsb8msb(bits: &mut [bool]) {
    for chunk in bits.chunks_mut(8) {
        chunk.reverse();
    }
}

fn read_bits(bits: &[bool], offset: &mut usize, len: usize) -> u32 {
    let mut v = 0u32;
    for _ in 0..len {
        v = (v << 1) | u32::from(bits.get(*offset).copied().unwrap_or(false));
        *offset += 1;
    }
    v
}

fn sch_frame_number(t1: u16, t2: u8, t3p: u8) -> u32 {
    let t3 = 10 * u32::from(t3p) + 1;
    let base = u32::from(t1) * 26 * 51;
    for k in 0..(26 * 51) {
        let fnr = base + k;
        if fnr % 26 == u32::from(t2) && fnr % 51 == t3 {
            return fnr;
        }
    }
    base + t3
}

fn corrected_sample(
    iq: &[Complex32],
    sym: usize,
    phase_samples: f64,
    samples_per_symbol: f64,
    carrier_step: f32,
) -> Complex32 {
    let pos = (phase_samples + sym as f64 * samples_per_symbol)
        .clamp(0.0, iq.len().saturating_sub(1) as f64);
    let i0 = pos.floor() as usize;
    let i1 = (i0 + 1).min(iq.len().saturating_sub(1));
    let frac = (pos - i0 as f64) as f32;
    let sample = iq[i0] * (1.0 - frac) + iq[i1] * frac;
    let phase = carrier_step * pos as f32;
    let rot = Complex32::new(phase.cos(), phase.sin());
    sample * rot
}

fn freq_to_shifted_bin(freq_hz: f64, sample_rate_hz: f64, n: usize) -> Option<usize> {
    if freq_hz < -sample_rate_hz / 2.0 || freq_hz > sample_rate_hz / 2.0 {
        return None;
    }
    let x = ((freq_hz + sample_rate_hz / 2.0) / sample_rate_hz) * n as f64;
    Some((x.round() as isize).clamp(0, n.saturating_sub(1) as isize) as usize)
}

fn bin_window(center: usize, radius: isize, n: usize) -> impl Iterator<Item = usize> {
    let n = n as isize;
    let center = center as isize;
    (-radius..=radius).map(move |d| (center + d).rem_euclid(n) as usize)
}

fn circular_distance(a: usize, b: usize, n: usize) -> usize {
    let d = a.abs_diff(b);
    d.min(n - d)
}
