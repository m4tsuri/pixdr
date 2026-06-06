use anyhow::Context;
use futuresdr::prelude::*;
use futuresdr::runtime::dev::prelude::*;
use log::{info, warn};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    apply_hann, apply_rx_config, current_fft_config, fft_to_db, smooth_spectrum,
    update_signal_products, update_state, SharedState, MAX_CONSTELLATION_POINTS,
};

/// Run the pixdr receive pipeline as a FutureSDR flowgraph.
///
/// The UHD device is a FutureSDR source block. Spectrum/waterfall generation is
/// a downstream sink block. The outer app worker only restarts this flowgraph
/// when radio configuration changes require a new UHD streamer.
pub fn run_b210_flowgraph(usrp: Arc<Mutex<uhd::Usrp>>, shared: SharedState) -> anyhow::Result<()> {
    futuresdr::runtime::init();

    let mut fg = Flowgraph::new();
    let src = UhdB210Source::new(usrp, shared.clone());
    let spectrum = SpectrumSink::new(shared);
    connect!(fg, src > spectrum);

    Runtime::new().run(fg)?;
    Ok(())
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
}

impl UhdB210Source<DefaultCpuWriter<Complex32>> {
    pub fn new(usrp: Arc<Mutex<uhd::Usrp>>, shared: SharedState) -> Self {
        Self {
            output: DefaultCpuWriter::default(),
            usrp,
            shared,
            streamer: None,
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
        let needs_reconfig = self
            .shared
            .lock()
            .map(|s| s.config_revision != s.applied_revision)
            .unwrap_or(false);
        if needs_reconfig {
            info!("FutureSDR UHD source stopping for retune");
            if let Some(streamer) = self.streamer.as_mut() {
                let _ = streamer.send_command(&uhd::StreamCommand {
                    time: uhd::StreamTime::Now,
                    command_type: uhd::StreamCommandType::StopContinuous,
                });
            }
            update_state(&self.shared, |s| {
                s.streaming = false;
                s.status = "Retuning".to_string();
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
                self.output.produce(n);
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

fn decimate_constellation(input: &[Complex32], max_points: usize) -> Vec<Complex32> {
    if input.is_empty() || max_points == 0 {
        return Vec::new();
    }
    let step = (input.len() / max_points).max(1);
    input.iter().step_by(step).take(max_points).copied().collect()
}
