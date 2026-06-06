use anyhow::Context;
use futuresdr::prelude::*;
use futuresdr::runtime::dev::prelude::*;
use log::{info, warn};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    apply_hann, apply_rx_config, current_fft_config, fft_to_db, gsm, smooth_spectrum,
    update_signal_products, update_state, SharedState, MAX_CONSTELLATION_POINTS,
};

/// Run the pixdr receive pipeline as a FutureSDR flowgraph.
///
/// The UHD device is a FutureSDR source block. Spectrum/waterfall generation is
/// a downstream sink block. The outer app worker only restarts this flowgraph
/// when radio configuration changes require a new UHD streamer.
pub fn run_b210_flowgraph(usrp: Arc<Mutex<uhd::Usrp>>, shared: SharedState) -> anyhow::Result<()> {
    futuresdr::runtime::init();

    let gsm_tx = spawn_gsm_analyzer_worker(shared.clone());
    let mut fg = Flowgraph::new();
    let src = UhdB210Source::new(usrp, shared.clone(), gsm_tx);
    let spectrum = SpectrumSink::new(shared.clone());
    connect!(fg, src > spectrum);

    Runtime::new().run(fg)?;
    Ok(())
}

fn spawn_gsm_analyzer_worker(shared: SharedState) -> mpsc::SyncSender<Vec<Complex32>> {
    let (tx, rx) = mpsc::sync_channel::<Vec<Complex32>>(4);
    std::thread::Builder::new()
        .name("pixdr-gsm-analyzer".to_string())
        .spawn(move || {
            let fft_size = 4096usize;
            let mut planner = FftPlanner::<f32>::new();
            let fft = planner.plan_fft_forward(fft_size);
            let mut fft_buf = vec![Complex32::new(0.0, 0.0); fft_size];
            let mut iq_history = Vec::<Complex32>::with_capacity(98_304);
            let mut last_publish = Instant::now() - Duration::from_millis(250);
            let mut last_verified_sch: Option<gsm::GsmSchDecode> = None;
            let mut sch_votes: Vec<SchVote> = Vec::new();
            let mut verified_sch_count = 0u32;
            let mut last_arfcn = None;
            let mut last_bcch_correlation: Option<f32> = None;
            let mut last_bcch_detected = false;
            let mut last_bcch_decode: Option<gsm::GsmBcchDecode> = None;
            let mut last_bcch_dump_key: Option<(Option<i32>, u64, bool)> = None;

            while let Ok(samples) = rx.recv() {
                append_gsm_history(&mut iq_history, &samples);
                while let Ok(samples) = rx.try_recv() {
                    append_gsm_history(&mut iq_history, &samples);
                }

                if last_publish.elapsed() < Duration::from_millis(250) || iq_history.len() < 512 {
                    continue;
                }

                let n = iq_history.len().min(fft_size);
                fft_buf.fill(Complex32::new(0.0, 0.0));
                let start = iq_history.len().saturating_sub(n);
                fft_buf[..n].copy_from_slice(&iq_history[start..]);
                apply_hann(&mut fft_buf[..n]);
                fft.process(&mut fft_buf);
                let spectrum = fft_to_db(&fft_buf);
                let (center_hz, sample_rate_hz, bandwidth_hz) = shared
                    .lock()
                    .map(|s| (s.center_hz, s.sample_rate, s.bandwidth_hz))
                    .unwrap_or((0.0, 0.0, 0.0));
                let mut analysis = gsm::analyze_pgsm900_downlink(
                    center_hz,
                    sample_rate_hz,
                    bandwidth_hz,
                    &spectrum,
                    &iq_history,
                );

                if analysis.arfcn != last_arfcn {
                    last_arfcn = analysis.arfcn;
                    last_verified_sch = None;
                    sch_votes.clear();
                    verified_sch_count = 0;
                    last_bcch_correlation = None;
                    last_bcch_detected = false;
                    last_bcch_decode = None;
                    last_bcch_dump_key = None;
                }

                let mut stable_verified_sch = None;
                let current_verified_sch = analysis
                    .sch_decode
                    .as_ref()
                    .filter(|sch| sch_confident_enough(sch) && is_valid_sch_frame_number(sch.frame_number))
                    .cloned();
                if let Some(sch) = current_verified_sch.clone() {
                    if let Some((locked, score)) = update_sch_votes(&mut sch_votes, sch) {
                        let changed_lock = last_verified_sch
                            .as_ref()
                            .map(|last| last.bsic != locked.bsic)
                            .unwrap_or(true);
                        if changed_lock {
                            last_bcch_correlation = None;
                            last_bcch_detected = false;
                            last_bcch_decode = None;
                            last_bcch_dump_key = None;
                        }
                        verified_sch_count = score;
                        last_verified_sch = Some(locked.clone());
                        stable_verified_sch = Some(locked);
                    }
                } else {
                    decay_sch_votes(&mut sch_votes);
                }

                // SCH symbol offsets are relative to the current rolling buffer.
                // Never use an old saved offset after the history window slides;
                // only run scheduled BCCH decode when this same analysis pass has
                // just verified stable SCH timing for the voted/locked BSIC.
                if let Some(sch) = stable_verified_sch.as_ref() {
                    let (corr, detected, decode) = gsm::decode_bcch_from_sch_timing(
                        center_hz,
                        sample_rate_hz,
                        analysis.carrier_hz,
                        sch.bcc,
                        &iq_history,
                        analysis.sch_symbol_offset,
                        analysis.sch_sample_phase,
                        true,
                    );
                    last_bcch_correlation = corr.or(last_bcch_correlation);
                    last_bcch_detected = last_bcch_detected || detected;
                    if let Some(decode) = decode {
                        let dump_key = (analysis.arfcn, decode.syndrome, decode.parity_ok);
                        if last_bcch_dump_key != Some(dump_key) {
                            write_bcch_debug_dump(
                                analysis.arfcn,
                                center_hz,
                                sample_rate_hz,
                                bandwidth_hz,
                                sch,
                                &decode,
                            );
                            last_bcch_dump_key = Some(dump_key);
                        }
                        last_bcch_decode = Some(decode);
                    }
                }

                let stable_sch_for_ui = if verified_sch_count >= 3 {
                    last_verified_sch.clone()
                } else {
                    None
                };
                analysis.sch_detected = stable_sch_for_ui.is_some();
                analysis.sch_decode = stable_sch_for_ui.clone();
                if stable_sch_for_ui.is_none() {
                    analysis.bcch_correlation = None;
                    analysis.bcch_detected = false;
                    analysis.bcch_decode = None;
                } else {
                    analysis.bcch_correlation = last_bcch_correlation;
                    analysis.bcch_detected = last_bcch_detected;
                    analysis.bcch_decode = last_bcch_decode.clone();
                }
                analysis.last_verified_sch = stable_sch_for_ui;
                analysis.verified_sch_count = verified_sch_count;
                update_state(&shared, |s| {
                    s.gsm900 = analysis;
                });
                last_publish = Instant::now();
            }
        })
        .expect("spawn GSM analyzer worker");
    tx
}

#[derive(Clone)]
struct SchVote {
    bsic: u8,
    bcc: u8,
    score: u32,
    sch: gsm::GsmSchDecode,
}

fn update_sch_votes(votes: &mut Vec<SchVote>, sch: gsm::GsmSchDecode) -> Option<(gsm::GsmSchDecode, u32)> {
    for vote in votes.iter_mut() {
        vote.score = vote.score.saturating_sub(1);
    }
    if let Some(vote) = votes.iter_mut().find(|vote| vote.bsic == sch.bsic && vote.bcc == sch.bcc) {
        vote.score = vote.score.saturating_add(3).min(12);
        vote.sch = sch;
    } else {
        votes.push(SchVote {
            bsic: sch.bsic,
            bcc: sch.bcc,
            score: 3,
            sch,
        });
    }
    votes.retain(|vote| vote.score > 0);
    votes
        .iter()
        .max_by_key(|vote| vote.score)
        .and_then(|vote| (vote.score >= 6).then(|| (vote.sch.clone(), vote.score)))
}

fn decay_sch_votes(votes: &mut Vec<SchVote>) {
    for vote in votes.iter_mut() {
        vote.score = vote.score.saturating_sub(1);
    }
    votes.retain(|vote| vote.score > 0);
}

fn sch_confident_enough(sch: &gsm::GsmSchDecode) -> bool {
    sch.parity_ok || sch.parity_syndrome.count_ones() <= 2
}

fn is_valid_sch_frame_number(frame_number: u32) -> bool {
    matches!(frame_number % 51, 1 | 11 | 21 | 31 | 41)
}

fn sch_frame_sequence_plausible(previous: u32, current: u32) -> bool {
    const GSM_HYPERFRAME: u32 = 26 * 51 * 2048;
    if current == previous {
        return true;
    }
    let delta = (current + GSM_HYPERFRAME - previous) % GSM_HYPERFRAME;
    delta > 0 && delta <= 520 && is_valid_sch_frame_number(current)
}

fn write_bcch_debug_dump(
    arfcn: Option<i32>,
    center_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    sch: &gsm::GsmSchDecode,
    bcch: &gsm::GsmBcchDecode,
) {
    let paths = [
        "/sdcard/Android/data/org.pixdr.app/files/pixdr_gsm_bcch_dump.txt",
        "/data/data/org.pixdr.app/files/pixdr_gsm_bcch_dump.txt",
    ];
    let content = format!(
        concat!(
            "pixdr GSM BCCH dump\n",
            "arfcn={:?}\ncenter_hz={:.3}\nsample_rate_hz={:.3}\nbandwidth_hz={:.3}\n",
            "sch_bsic={} sch_ncc={} sch_bcc={} sch_fn={} sch_metric={:.6}\n",
            "bcch_parity_ok={} bcch_syndrome=0x{:010x} bcch_metric={:.6}\n",
            "bcch_message_type={:?} bcch_message_name={} bcch_l2_hex={}\n",
            "c_bits_456={}\n",
            "u_bits_228={}\n"
        ),
        arfcn,
        center_hz,
        sample_rate_hz,
        bandwidth_hz,
        sch.bsic,
        sch.ncc,
        sch.bcc,
        sch.frame_number,
        sch.path_metric,
        bcch.parity_ok,
        bcch.syndrome,
        bcch.path_metric,
        bcch.message_type,
        bcch.message_name,
        bcch.l2_hex,
        bcch.c_bits,
        bcch.u_bits,
    );
    let mut wrote = false;
    for path in paths {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, &content) {
            Ok(()) => {
                info!("wrote GSM BCCH debug dump to {path}");
                wrote = true;
            }
            Err(err) => warn!("failed to write GSM BCCH debug dump to {path}: {err}"),
        }
    }
    if !wrote {
        warn!("failed to write GSM BCCH debug dump to all paths");
    }
}

fn append_gsm_history(history: &mut Vec<Complex32>, samples: &[Complex32]) {
    const GSM_IQ_HISTORY_MAX: usize = 98_304;
    history.extend_from_slice(samples);
    if history.len() > GSM_IQ_HISTORY_MAX {
        let overflow = history.len() - GSM_IQ_HISTORY_MAX;
        history.drain(..overflow);
    }
}

#[derive(Block)]
#[blocking]
#[type_name(UhdB210Source)]
pub struct UhdB210Source<OUT = DefaultCpuWriter<Complex32>>
where
    OUT: CpuBufferWriter<Item = Complex32>,
{
    #[output]
    output: OUT,
    usrp: Arc<Mutex<uhd::Usrp>>,
    shared: SharedState,
    streamer: Option<uhd::ReceiveStreamer<'static, Complex32>>,
    gsm_tx: mpsc::SyncSender<Vec<Complex32>>,
}

impl UhdB210Source<DefaultCpuWriter<Complex32>> {
    pub fn new(
        usrp: Arc<Mutex<uhd::Usrp>>,
        shared: SharedState,
        gsm_tx: mpsc::SyncSender<Vec<Complex32>>,
    ) -> Self {
        Self {
            output: DefaultCpuWriter::default(),
            usrp,
            shared,
            streamer: None,
            gsm_tx,
        }
    }
}

#[doc(hidden)]
impl<OUT> Kernel for UhdB210Source<OUT>
where
    OUT: CpuBufferWriter<Item = Complex32>,
{
    async fn init(&mut self, _mo: &mut MessageOutputs, _meta: &mut BlockMeta) -> Result<()> {
        let mut usrp = self.usrp.lock().unwrap();
        let _ = usrp.set_rx_antenna("RX2", 0);
        let _ = usrp.set_rx_dc_offset_enabled(true, 0);
        apply_rx_config(&mut usrp, &self.shared)?;

        let args = uhd::StreamArgs::<Complex32>::builder()
            .channels(vec![0])
            .build();
        let mut streamer = usrp
            .get_rx_stream(&args)
            .context("get_rx_stream(channel=0, fc32/sc16) failed")?;
        streamer
            .send_command(&uhd::StreamCommand {
                time: uhd::StreamTime::Now,
                command_type: uhd::StreamCommandType::StartContinuous,
            })
            .context("start continuous RX streaming failed")?;

        // uhd-rs models ReceiveStreamer as borrowing the Usrp. This block keeps
        // the Usrp alive through Arc<Mutex<Usrp>> and explicitly drops/stops the
        // streamer in deinit before releasing the block's Arc clone.
        let streamer = unsafe {
            std::mem::transmute::<
                uhd::ReceiveStreamer<'_, Complex32>,
                uhd::ReceiveStreamer<'static, Complex32>,
            >(streamer)
        };
        self.streamer = Some(streamer);

        info!("FutureSDR UHD B210 source active");
        update_state(&self.shared, |s| {
            s.status = "Streaming".to_string();
            s.detail = "FutureSDR UHD source active".to_string();
            s.streaming = true;
        });
        Ok(())
    }

    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &mut BlockMeta,
    ) -> Result<()> {
        let (needs_reconfig, user_paused, user_stopped) = self
            .shared
            .lock()
            .map(|s| (s.config_revision != s.applied_revision, s.user_paused, s.user_stopped))
            .unwrap_or((false, false, false));
        if needs_reconfig || user_paused || user_stopped {
            let reason = if user_stopped { "stop" } else if user_paused { "pause" } else { "retune" };
            info!("FutureSDR UHD source stopping for {reason}");
            if let Some(streamer) = self.streamer.as_mut() {
                let _ = streamer.send_command(&uhd::StreamCommand {
                    time: uhd::StreamTime::Now,
                    command_type: uhd::StreamCommandType::StopContinuous,
                });
            }
            update_state(&self.shared, |s| {
                s.streaming = false;
                s.status = if user_stopped {
                    "Stopping"
                } else if user_paused {
                    "Paused"
                } else {
                    "Retuning"
                }
                .to_string();
            });
            io.finished = true;
            return Ok(());
        }

        let out = self.output.slice();
        if out.is_empty() {
            return Ok(());
        }

        let streamer = self.streamer.as_mut().context("UHD RX streamer not initialized")?;
        match streamer.receive_simple(out) {
            Ok(md) => {
                let n = md.samples().min(out.len());
                let gsm_samples = if n > 0 { Some(out[..n].to_vec()) } else { None };
                self.output.produce(n);
                if let Some(samples) = gsm_samples {
                    let _ = self.gsm_tx.try_send(samples);
                }
            }
            Err(e) => {
                warn!("FutureSDR UHD source receive failed: {e}");
                update_state(&self.shared, |s| {
                    s.streaming = false;
                    s.status = "B210 opened; RX stream failed".to_string();
                    s.detail = format!("No live samples: {e}");
                });
                io.finished = true;
            }
        }
        io.call_again = true;
        Ok(())
    }

    async fn deinit(&mut self, _mo: &mut MessageOutputs, _meta: &mut BlockMeta) -> Result<()> {
        if let Some(mut streamer) = self.streamer.take() {
            let _ = streamer.send_command(&uhd::StreamCommand {
                time: uhd::StreamTime::Now,
                command_type: uhd::StreamCommandType::StopContinuous,
            });
        }
        Ok(())
    }
}

#[derive(Block)]
#[type_name(SpectrumSink)]
pub struct SpectrumSink<I = DefaultCpuReader<Complex32>>
where
    I: CpuBufferReader<Item = Complex32>,
{
    #[input]
    input: I,
    shared: SharedState,
    planner: FftPlanner<f32>,
    fft: Arc<dyn Fft<f32>>,
    fft_size: usize,
    publish_interval: Duration,
    smoothing: f32,
    applied_fft_revision: u64,
    fft_buf: Vec<Complex32>,
    smoothed_spectrum: Vec<f32>,
    last_publish: Instant,
}

impl SpectrumSink<DefaultCpuReader<Complex32>> {
    pub fn new(shared: SharedState) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let (fft_size, publish_interval, smoothing, applied_fft_revision) =
            current_fft_config(&shared);
        let fft = planner.plan_fft_forward(fft_size);
        update_state(&shared, |s| s.applied_fft_revision = applied_fft_revision);
        Self {
            input: DefaultCpuReader::default(),
            shared,
            planner,
            fft,
            fft_size,
            publish_interval,
            smoothing,
            applied_fft_revision,
            fft_buf: vec![Complex32::new(0.0, 0.0); fft_size],
            smoothed_spectrum: vec![-100.0; fft_size],
            last_publish: Instant::now() - publish_interval,
        }
    }
}

#[doc(hidden)]
impl<I> Kernel for SpectrumSink<I>
where
    I: CpuBufferReader<Item = Complex32>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &mut BlockMeta,
    ) -> Result<()> {
        let input = self.input.slice();
        let input_len = input.len();

        let (new_fft_size, new_interval, new_smoothing, new_fft_revision) =
            current_fft_config(&self.shared);
        if new_fft_revision != self.applied_fft_revision {
            self.fft_size = new_fft_size;
            self.publish_interval = new_interval;
            self.smoothing = new_smoothing;
            self.applied_fft_revision = new_fft_revision;
            self.fft = self.planner.plan_fft_forward(self.fft_size);
            self.fft_buf.resize(self.fft_size, Complex32::new(0.0, 0.0));
            self.smoothed_spectrum.resize(self.fft_size, -100.0);
            update_state(&self.shared, |s| {
                s.applied_fft_revision = self.applied_fft_revision;
                s.spectrum_db.resize(self.fft_size, -100.0);
                s.waterfall.clear();
                s.detail = format!(
                    "FFT: {} bins @ {:.1} FPS",
                    self.fft_size,
                    1.0 / self.publish_interval.as_secs_f32()
                );
            });
        }

        if input_len > 0 && self.last_publish.elapsed() >= self.publish_interval {
            let n = input_len.min(self.fft_size);
            self.fft_buf.fill(Complex32::new(0.0, 0.0));
            self.fft_buf[..n].copy_from_slice(&input[..n]);
            apply_hann(&mut self.fft_buf[..n]);
            self.fft.process(&mut self.fft_buf);
            let constellation = decimate_constellation(input, MAX_CONSTELLATION_POINTS);
            let spectrum = fft_to_db(&self.fft_buf);
            smooth_spectrum(&spectrum, &mut self.smoothed_spectrum, self.smoothing);
            update_signal_products(
                &self.shared,
                self.smoothed_spectrum.clone(),
                constellation,
            );
            self.last_publish = Instant::now();
        }

        self.input.consume(input_len);
        if self.input.finished() {
            io.finished = true;
        }
        Ok(())
    }
}

struct GsmBcchDecodeJob {
    arfcn: Option<i32>,
    center_hz: f64,
    sample_rate_hz: f64,
    carrier_hz: f64,
    bcc: u8,
    sch_symbol_offset: Option<f32>,
    sch_sample_phase: Option<f32>,
    iq: Vec<Complex32>,
}

struct GsmBcchDecodeResult {
    arfcn: Option<i32>,
    decode: Option<gsm::GsmBcchDecode>,
}

#[derive(Block)]
#[type_name(Gsm900AnalyzerSink)]
pub struct Gsm900AnalyzerSink<I = DefaultCpuReader<Complex32>>
where
    I: CpuBufferReader<Item = Complex32>,
{
    #[input]
    input: I,
    shared: SharedState,
    fft: Arc<dyn Fft<f32>>,
    fft_size: usize,
    fft_buf: Vec<Complex32>,
    iq_history: Vec<Complex32>,
    last_publish: Instant,
    last_verified_sch: Option<gsm::GsmSchDecode>,
    last_verified_sch_symbol_offset: Option<f32>,
    last_verified_sch_sample_phase: Option<f32>,
    verified_sch_count: u32,
    last_arfcn: Option<i32>,
    bcch_decode_tx: mpsc::SyncSender<GsmBcchDecodeJob>,
    bcch_decode_rx: mpsc::Receiver<GsmBcchDecodeResult>,
    bcch_decode_pending: bool,
    last_bcch_decode: Option<gsm::GsmBcchDecode>,
}

impl Gsm900AnalyzerSink<DefaultCpuReader<Complex32>> {
    pub fn new(shared: SharedState) -> Self {
        let fft_size = 4096usize;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        let (job_tx, job_rx) = mpsc::sync_channel::<GsmBcchDecodeJob>(1);
        let (result_tx, result_rx) = mpsc::sync_channel::<GsmBcchDecodeResult>(1);
        std::thread::Builder::new()
            .name("pixdr-gsm-bcch-decode".to_string())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let (_, _, decode) = gsm::decode_bcch_from_sch_timing(
                        job.center_hz,
                        job.sample_rate_hz,
                        job.carrier_hz,
                        job.bcc,
                        &job.iq,
                        job.sch_symbol_offset,
                        job.sch_sample_phase,
                        true,
                    );
                    let _ = result_tx.send(GsmBcchDecodeResult {
                        arfcn: job.arfcn,
                        decode,
                    });
                }
            })
            .expect("spawn GSM BCCH decode worker");
        Self {
            input: DefaultCpuReader::default(),
            shared,
            fft,
            fft_size,
            fft_buf: vec![Complex32::new(0.0, 0.0); fft_size],
            iq_history: Vec::with_capacity(98_304),
            last_publish: Instant::now() - Duration::from_millis(250),
            last_verified_sch: None,
            last_verified_sch_symbol_offset: None,
            last_verified_sch_sample_phase: None,
            verified_sch_count: 0,
            last_arfcn: None,
            bcch_decode_tx: job_tx,
            bcch_decode_rx: result_rx,
            bcch_decode_pending: false,
            last_bcch_decode: None,
        }
    }
}

#[doc(hidden)]
impl<I> Kernel for Gsm900AnalyzerSink<I>
where
    I: CpuBufferReader<Item = Complex32>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &mut BlockMeta,
    ) -> Result<()> {
        let input = self.input.slice();
        let input_len = input.len();

        if input_len > 0 {
            self.iq_history.extend_from_slice(input);
            const GSM_IQ_HISTORY_MAX: usize = 98_304;
            if self.iq_history.len() > GSM_IQ_HISTORY_MAX {
                let overflow = self.iq_history.len() - GSM_IQ_HISTORY_MAX;
                self.iq_history.drain(..overflow);
            }
        }

        // Important: consume the FutureSDR input before doing GSM analysis.
        // The GSM path is diagnostic/control-plane work; it must never apply
        // backpressure to UhdB210Source, otherwise the B210 RX stream can stop.
        self.input.consume(input_len);

        if input_len > 0 && self.last_publish.elapsed() >= Duration::from_millis(900) {
            let n = self.iq_history.len().min(self.fft_size);
            self.fft_buf.fill(Complex32::new(0.0, 0.0));
            let start = self.iq_history.len().saturating_sub(n);
            self.fft_buf[..n].copy_from_slice(&self.iq_history[start..]);
            apply_hann(&mut self.fft_buf[..n]);
            self.fft.process(&mut self.fft_buf);
            let spectrum = fft_to_db(&self.fft_buf);
            let (center_hz, sample_rate_hz, bandwidth_hz) = self
                .shared
                .lock()
                .map(|s| (s.center_hz, s.sample_rate, s.bandwidth_hz))
                .unwrap_or((0.0, 0.0, 0.0));
            let mut analysis = gsm::analyze_pgsm900_downlink(
                center_hz,
                sample_rate_hz,
                bandwidth_hz,
                &spectrum,
                &self.iq_history,
            );
            while let Ok(result) = self.bcch_decode_rx.try_recv() {
                self.bcch_decode_pending = false;
                if result.arfcn == self.last_arfcn {
                    self.last_bcch_decode = result.decode;
                }
            }

            if analysis.arfcn != self.last_arfcn {
                self.last_arfcn = analysis.arfcn;
                self.last_verified_sch = None;
                self.last_verified_sch_symbol_offset = None;
                self.last_verified_sch_sample_phase = None;
                self.verified_sch_count = 0;
                self.bcch_decode_pending = false;
                self.last_bcch_decode = None;
            }
            if let Some(sch) = analysis.sch_decode.as_ref().filter(|sch| sch.parity_ok).cloned() {
                self.last_verified_sch = Some(sch);
                self.last_verified_sch_symbol_offset = analysis.sch_symbol_offset;
                self.last_verified_sch_sample_phase = analysis.sch_sample_phase;
                self.verified_sch_count = self.verified_sch_count.saturating_add(1);
            }
            if let Some(sch) = self.last_verified_sch.as_ref() {
                let (corr, detected, decode) = gsm::decode_bcch_from_sch_timing(
                    center_hz,
                    sample_rate_hz,
                    analysis.carrier_hz,
                    sch.bcc,
                    &self.iq_history,
                    self.last_verified_sch_symbol_offset,
                    self.last_verified_sch_sample_phase,
                    false,
                );
                analysis.bcch_correlation = corr;
                analysis.bcch_detected = detected;
                if detected && !self.bcch_decode_pending {
                    let job = GsmBcchDecodeJob {
                        arfcn: analysis.arfcn,
                        center_hz,
                        sample_rate_hz,
                        carrier_hz: analysis.carrier_hz,
                        bcc: sch.bcc,
                        sch_symbol_offset: self.last_verified_sch_symbol_offset,
                        sch_sample_phase: self.last_verified_sch_sample_phase,
                        iq: self.iq_history.clone(),
                    };
                    if self.bcch_decode_tx.try_send(job).is_ok() {
                        self.bcch_decode_pending = true;
                    }
                }
                analysis.bcch_decode = self.last_bcch_decode.clone().or(decode);
            }
            analysis.last_verified_sch = self.last_verified_sch.clone();
            analysis.verified_sch_count = self.verified_sch_count;
            update_state(&self.shared, |s| {
                s.gsm900 = analysis;
            });
            self.last_publish = Instant::now();
        }

        if self.input.finished() {
            io.finished = true;
        }
        Ok(())
    }
}

fn decimate_constellation(input: &[Complex32], max_points: usize) -> Vec<Complex32> {
    if input.is_empty() || max_points == 0 {
        return Vec::new();
    }
    let step = (input.len() / max_points).max(1);
    input.iter().step_by(step).take(max_points).copied().collect()
}
