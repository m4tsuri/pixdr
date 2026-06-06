use anyhow::Context;
use futuresdr::prelude::*;
use futuresdr::runtime::dev::prelude::*;
use log::{info, warn};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    apply_hann, apply_rx_config, apply_tx_config, current_fft_config, fft_to_db,
    mute_tx_output, smooth_spectrum, tx_image_snapshot, update_signal_products, update_state, SharedState,
    TxImageSnapshot, MAX_CONSTELLATION_POINTS,
};

/// Run the pixdr receive pipeline as a FutureSDR flowgraph.
///
/// The UHD device is a FutureSDR source block. Spectrum/waterfall generation is
/// a downstream sink block. The outer app worker only restarts this flowgraph
/// when radio configuration changes require a new UHD streamer.
pub fn run_b210_flowgraph(usrp: Arc<Mutex<uhd::Usrp>>, shared: SharedState) -> anyhow::Result<()> {
    futuresdr::runtime::init();

    let enable_tx_branch = shared.lock().map(|s| s.tx_enabled).unwrap_or(false);
    update_state(&shared, |s| {
        s.applied_graph_revision = s.graph_revision;
    });
    let mut fg = Flowgraph::new();
    let rx_src = UhdB210Source::new(usrp.clone(), shared.clone());
    let spectrum = SpectrumSink::new(shared.clone());
    connect!(fg, rx_src > spectrum);
    if enable_tx_branch {
        let tx_src = ImageSpectrumSource::new(shared.clone());
        let tx_sink = UhdB210Sink::new(usrp, shared);
        connect!(fg, tx_src > tx_sink);
    }

    Runtime::new().run(fg)?;
    Ok(())
}

#[derive(Block)]
#[type_name(ImageSpectrumSource)]
pub struct ImageSpectrumSource<OUT = DefaultCpuWriter<Complex32>>
where
    OUT: CpuBufferWriter<Item = Complex32>,
{
    #[output]
    output: OUT,
    shared: SharedState,
    samples: Vec<Complex32>,
    cursor: usize,
    loop_enabled: bool,
    revision: u64,
    applied_graph_revision: u64,
    completed: bool,
}

impl ImageSpectrumSource<DefaultCpuWriter<Complex32>> {
    pub fn new(shared: SharedState) -> Self {
        Self {
            output: DefaultCpuWriter::default(),
            shared,
            samples: Vec::new(),
            cursor: 0,
            loop_enabled: false,
            revision: 0,
            applied_graph_revision: 0,
            completed: false,
        }
    }
}

#[doc(hidden)]
impl<OUT> Kernel for ImageSpectrumSource<OUT>
where
    OUT: CpuBufferWriter<Item = Complex32>,
{
    async fn init(&mut self, _mo: &mut MessageOutputs, _meta: &mut BlockMeta) -> Result<()> {
        self.applied_graph_revision = self.shared.lock().map(|s| s.graph_revision).unwrap_or(0);
        Ok(())
    }

    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &mut BlockMeta,
    ) -> Result<()> {
        let (tx_enabled, user_stopped, graph_restart, current_revision) = self
            .shared
            .lock()
            .map(|s| {
                (
                    s.tx_enabled,
                    s.user_stopped,
                    s.user_paused || s.graph_revision != self.applied_graph_revision,
                    s.tx_revision,
                )
            })
            .unwrap_or((false, false, false, self.revision));
        if user_stopped || graph_restart || self.completed {
            io.finished = true;
            return Ok(());
        }
        if !tx_enabled {
            thread::sleep(Duration::from_millis(20));
            io.call_again = true;
            return Ok(());
        }
        if current_revision != self.revision || self.samples.is_empty() {
            match tx_image_snapshot(&self.shared).and_then(|snapshot| {
                let samples = generate_image_spectrogram_iq(&snapshot)?;
                Ok((snapshot, samples))
            }) {
                Ok((snapshot, samples)) => {
                    self.samples = samples;
                    self.cursor = 0;
                    self.loop_enabled = snapshot.loop_enabled;
                    self.revision = snapshot.revision;
                    update_state(&self.shared, |s| {
                        s.detail = format!("Generated {} TX samples from image", self.samples.len());
                    });
                }
                Err(e) => {
                    update_state(&self.shared, |s| {
                        s.tx_enabled = false;
                        s.tx_transmitting = false;
                        s.detail = format!("Could not prepare TX image: {e}");
                    });
                    thread::sleep(Duration::from_millis(20));
                    io.call_again = true;
                    return Ok(());
                }
            }
        }

        let out = self.output.slice();
        if out.is_empty() {
            return Ok(());
        }

        let mut produced = 0;
        while produced < out.len() {
            if self.cursor >= self.samples.len() {
                if self.loop_enabled {
                    self.cursor = 0;
                } else {
                    self.completed = true;
                    update_state(&self.shared, |s| {
                        s.tx_enabled = false;
                        s.tx_transmitting = true;
                        s.status = if s.streaming { "Streaming" } else { "B210 opened" }.to_string();
                        s.detail = "TX image complete; flushing burst".to_string();
                    });
                    break;
                }
            }
            let available = self.samples.len().saturating_sub(self.cursor);
            if available == 0 {
                break;
            }
            let n = available.min(out.len() - produced);
            out[produced..produced + n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            produced += n;
        }

        self.output.produce(produced);
        if self.completed {
            io.finished = true;
        } else if produced == 0 {
            thread::sleep(Duration::from_millis(10));
            io.call_again = true;
        } else {
            io.call_again = true;
        }
        Ok(())
    }
}

fn generate_image_spectrogram_iq(snapshot: &TxImageSnapshot) -> anyhow::Result<Vec<Complex32>> {
    let fft_size = snapshot.width.next_power_of_two().max(512);
    let row_samples = ((snapshot.sample_rate * snapshot.row_ms / 1000.0).round() as usize)
        .max(fft_size)
        .min(fft_size * 256);
    let repeats = row_samples.div_ceil(fft_size).max(1);
    let mut planner = FftPlanner::<f32>::new();
    let ifft = planner.plan_fft_inverse(fft_size);
    let mut spectrum = vec![Complex32::new(0.0, 0.0); fft_size];
    let mut time = vec![Complex32::new(0.0, 0.0); fft_size];
    let mut samples = Vec::with_capacity(snapshot.height * repeats * fft_size);
    let amp = snapshot.amplitude as f32;

    for row in 0..snapshot.height {
        // Waterfalls usually scroll with newest rows at the opposite edge of a
        // normal image. Emit bottom image rows first so the received spectrogram
        // appears upright.
        let image_row = snapshot.height - 1 - row;
        spectrum.fill(Complex32::new(0.0, 0.0));
        let mut row_max = 0.0_f32;
        for x in 0..snapshot.width {
            let pixel = snapshot.gray[image_row * snapshot.width + x] as f32 / 255.0;
            if pixel <= 0.01 {
                continue;
            }
            let centered = x as isize - snapshot.width as isize / 2;
            // Only keep a small DC guard when TX LO offset tuning is disabled.
            // With off tuning enabled, the analog LO leakage is moved away from
            // the desired image center, so blanking bin 0 would create an
            // artificial vertical seam in the transmitted picture.
            if snapshot.lo_offset_hz.abs() <= 1.0 && centered.abs() <= 1 {
                continue;
            }
            row_max = row_max.max(pixel);
            let mag = pixel;
            let bin = if centered >= 0 {
                centered as usize
            } else {
                fft_size - ((-centered) as usize)
            } % fft_size;
            let phase = (((row * 131 + x * 17) % 6283) as f32) * 0.001;
            spectrum[bin] = Complex32::from_polar(mag, phase);
        }
        if row_max <= 0.01 {
            time.fill(Complex32::new(0.0, 0.0));
        } else {
            time.copy_from_slice(&spectrum);
            ifft.process(&mut time);
            let scale = 1.0 / fft_size as f32;
            for sample in &mut time {
                *sample *= scale;
            }
            let peak = time.iter().map(|s| s.norm()).fold(0.0_f32, f32::max).max(1.0e-9);
            let target_peak = amp * row_max;
            let norm = target_peak / peak;
            for sample in &mut time {
                *sample *= norm;
            }
        }
        for _ in 0..repeats {
            samples.extend(time.iter().copied());
        }
    }

    if samples.is_empty() {
        anyhow::bail!("Generated empty TX image waveform");
    }
    Ok(samples)
}

#[derive(Block)]
#[blocking]
#[type_name(UhdB210Sink)]
pub struct UhdB210Sink<I = DefaultCpuReader<Complex32>>
where
    I: CpuBufferReader<Item = Complex32>,
{
    #[input]
    input: I,
    usrp: Arc<Mutex<uhd::Usrp>>,
    shared: SharedState,
    streamer: Option<uhd::TransmitStreamer<'static, Complex32>>,
    revision: u64,
    applied_graph_revision: u64,
    in_burst: bool,
    pending_eob: bool,
}

impl UhdB210Sink<DefaultCpuReader<Complex32>> {
    pub fn new(usrp: Arc<Mutex<uhd::Usrp>>, shared: SharedState) -> Self {
        Self {
            input: DefaultCpuReader::default(),
            usrp,
            shared,
            streamer: None,
            revision: 0,
            applied_graph_revision: 0,
            in_burst: false,
            pending_eob: false,
        }
    }
}

impl<I> UhdB210Sink<I>
where
    I: CpuBufferReader<Item = Complex32>,
{
    fn finish_burst_and_mute(&mut self) {
        if self.in_burst || self.pending_eob {
            if let Some(streamer) = self.streamer.as_mut() {
                let _ = streamer.send_end_of_burst(0.1);
            }
        }
        self.in_burst = false;
        self.pending_eob = false;
        self.streamer.take();
        if let Ok(mut usrp) = self.usrp.lock() {
            let _ = mute_tx_output(&mut usrp, &self.shared);
        }
    }
}

#[doc(hidden)]
impl<I> Kernel for UhdB210Sink<I>
where
    I: CpuBufferReader<Item = Complex32>,
{
    async fn init(&mut self, _mo: &mut MessageOutputs, _meta: &mut BlockMeta) -> Result<()> {
        self.applied_graph_revision = self.shared.lock().map(|s| s.graph_revision).unwrap_or(0);
        let tx_channel = self.shared.lock().map(|s| s.tx_channel).unwrap_or(0);
        let mut usrp = self.usrp.lock().unwrap();
        self.revision = apply_tx_config(&mut usrp, &self.shared)?;
        let args = uhd::StreamArgs::<Complex32>::builder()
            .channels(vec![tx_channel])
            .build();
        let streamer = usrp
            .get_tx_stream(&args)
            .context("get_tx_stream(fc32/sc16) failed")?;
        let streamer = unsafe {
            std::mem::transmute::<
                uhd::TransmitStreamer<'_, Complex32>,
                uhd::TransmitStreamer<'static, Complex32>,
            >(streamer)
        };
        self.streamer = Some(streamer);
        Ok(())
    }

    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &mut BlockMeta,
    ) -> Result<()> {
        let (tx_enabled, user_stopped, graph_restart, current_revision) = self
            .shared
            .lock()
            .map(|s| {
                (
                    s.tx_enabled,
                    s.user_stopped,
                    s.user_paused || s.graph_revision != self.applied_graph_revision,
                    s.tx_revision,
                )
            })
            .unwrap_or((false, false, false, self.revision));
        if user_stopped || graph_restart {
            self.finish_burst_and_mute();
            io.finished = true;
            return Ok(());
        }
        if current_revision != self.revision {
            self.finish_burst_and_mute();
            let tx_channel = self.shared.lock().map(|s| s.tx_channel).unwrap_or(0);
            let mut usrp = self.usrp.lock().unwrap();
            self.revision = apply_tx_config(&mut usrp, &self.shared)?;
            let args = uhd::StreamArgs::<Complex32>::builder()
                .channels(vec![tx_channel])
                .build();
            let streamer = usrp
                .get_tx_stream(&args)
                .context("get_tx_stream(fc32/sc16) failed")?;
            self.streamer = Some(unsafe {
                std::mem::transmute::<
                    uhd::TransmitStreamer<'_, Complex32>,
                    uhd::TransmitStreamer<'static, Complex32>,
                >(streamer)
            });
        }

        let input_finished = self.input.finished();
        let input = self.input.slice();
        let input_len = input.len();
        if input.is_empty() {
            if input_finished || !tx_enabled || self.pending_eob {
                self.finish_burst_and_mute();
                update_state(&self.shared, |s| {
                    s.tx_transmitting = false;
                    s.status = if s.streaming { "Streaming" } else { "B210 opened" }.to_string();
                    if s.detail == "TX image complete; flushing burst" {
                        s.detail = "TX image complete".to_string();
                    }
                });
                io.finished = true;
            } else {
                thread::sleep(Duration::from_millis(10));
                io.call_again = true;
            }
            return Ok(());
        }

        let streamer = self.streamer.as_mut().context("UHD TX streamer not initialized")?;
        let final_packet = input_finished || !tx_enabled;
        let metadata = uhd::TransmitMetadata::with_burst_flags(!self.in_burst, final_packet);
        match streamer.transmit_with_metadata(&mut [input], metadata, 0.1) {
            Ok(md) => {
                let n = md.samples().min(input_len);
                self.input.consume(n);
                if n > 0 {
                    self.in_burst = !final_packet;
                    self.pending_eob = final_packet;
                }
                update_state(&self.shared, |s| {
                    s.tx_transmitting = true;
                    s.status = "Transmitting".to_string();
                });
                if final_packet && n == input_len {
                    self.finish_burst_and_mute();
                    update_state(&self.shared, |s| {
                        s.tx_transmitting = false;
                        s.status = if s.streaming { "Streaming" } else { "B210 opened" }.to_string();
                        if s.detail == "TX image complete; flushing burst" {
                            s.detail = "TX image complete".to_string();
                        }
                    });
                }
                if n == 0 {
                    io.call_again = true;
                }
            }
            Err(e) => {
                warn!("FutureSDR UHD sink transmit failed: {e}");
                self.in_burst = false;
                self.pending_eob = false;
                update_state(&self.shared, |s| {
                    s.tx_enabled = false;
                    s.tx_transmitting = false;
                    s.status = "B210 opened; TX failed".to_string();
                    s.detail = format!("TX send failed: {e}");
                });
            }
        }
        Ok(())
    }

    async fn deinit(&mut self, _mo: &mut MessageOutputs, _meta: &mut BlockMeta) -> Result<()> {
        self.finish_burst_and_mute();
        update_state(&self.shared, |s| {
            s.tx_transmitting = false;
        });
        Ok(())
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
    applied_center_hz: f64,
    applied_bandwidth_hz: f64,
    applied_sample_rate: f64,
    applied_gain_db: f64,
    applied_rx_channel: usize,
    applied_config_revision: u64,
    applied_graph_revision: u64,
}

impl UhdB210Source<DefaultCpuWriter<Complex32>> {
    pub fn new(usrp: Arc<Mutex<uhd::Usrp>>, shared: SharedState) -> Self {
        Self {
            output: DefaultCpuWriter::default(),
            usrp,
            shared,
            streamer: None,
            applied_center_hz: 0.0,
            applied_bandwidth_hz: 0.0,
            applied_sample_rate: 0.0,
            applied_gain_db: 0.0,
            applied_rx_channel: 0,
            applied_config_revision: 0,
            applied_graph_revision: 0,
        }
    }
}

#[doc(hidden)]
impl<OUT> Kernel for UhdB210Source<OUT>
where
    OUT: CpuBufferWriter<Item = Complex32>,
{
    async fn init(&mut self, _mo: &mut MessageOutputs, _meta: &mut BlockMeta) -> Result<()> {
        let rx_channel = self.shared.lock().map(|s| s.rx_channel).unwrap_or(0);
        let mut usrp = self.usrp.lock().unwrap();
        let _ = usrp.set_rx_dc_offset_enabled(true, rx_channel);
        apply_rx_config(&mut usrp, &self.shared)?;
        {
            let state = self.shared.lock().unwrap();
            self.applied_center_hz = state.center_hz;
            self.applied_bandwidth_hz = state.bandwidth_hz.min(state.sample_rate);
            self.applied_sample_rate = state.sample_rate;
            self.applied_gain_db = state.gain_db;
            self.applied_rx_channel = state.rx_channel;
            self.applied_config_revision = state.config_revision;
            self.applied_graph_revision = state.graph_revision;
        }
        update_state(&self.shared, |s| {
            s.applied_graph_revision = self.applied_graph_revision;
        });

        let args = uhd::StreamArgs::<Complex32>::builder()
            .channels(vec![rx_channel])
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
        let (
            center_hz,
            bandwidth_hz,
            sample_rate,
            gain_db,
            rx_channel,
            config_revision,
            graph_revision,
            user_paused,
            user_stopped,
        ) = self
            .shared
            .lock()
            .map(|s| {
                (
                    s.center_hz,
                    s.bandwidth_hz.min(s.sample_rate),
                    s.sample_rate,
                    s.gain_db,
                    s.rx_channel,
                    s.config_revision,
                    s.graph_revision,
                    s.user_paused,
                    s.user_stopped,
                )
            })
            .unwrap_or((
                self.applied_center_hz,
                self.applied_bandwidth_hz,
                self.applied_sample_rate,
                self.applied_gain_db,
                self.applied_rx_channel,
                self.applied_config_revision,
                self.applied_graph_revision,
                false,
                false,
            ));

        let needs_graph_restart = graph_revision != self.applied_graph_revision
            || rx_channel != self.applied_rx_channel
            || (sample_rate - self.applied_sample_rate).abs() > 1.0;
        if needs_graph_restart || user_paused || user_stopped {
            let reason = if user_stopped {
                "stop"
            } else if user_paused {
                "pause"
            } else if graph_revision != self.applied_graph_revision {
                "graph change"
            } else {
                "stream reconfig"
            };
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

        if config_revision != self.applied_config_revision
            || (center_hz - self.applied_center_hz).abs() > 1.0
            || (bandwidth_hz - self.applied_bandwidth_hz).abs() > 1.0
            || (gain_db - self.applied_gain_db).abs() > 0.01
        {
            info!(
                "Hot applying RX config rev={config_revision}: ch={} center={:.6} MHz bandwidth={:.3} MHz gain={:.1} dB",
                rx_channel,
                center_hz / 1e6,
                bandwidth_hz / 1e6,
                gain_db
            );
            {
                let mut usrp = self.usrp.lock().unwrap();
                if (bandwidth_hz - self.applied_bandwidth_hz).abs() > 1.0 {
                    usrp
                        .set_rx_bandwidth(bandwidth_hz, rx_channel)
                        .with_context(|| format!("hot set_rx_bandwidth(ch={rx_channel}) failed"))?;
                }
                if (center_hz - self.applied_center_hz).abs() > 1.0 {
                    usrp
                        .set_rx_frequency(&uhd::TuneRequest::with_frequency(center_hz), rx_channel)
                        .with_context(|| format!("hot set_rx_frequency(ch={rx_channel}) failed"))?;
                }
                if (gain_db - self.applied_gain_db).abs() > 0.01 {
                    usrp
                        .set_rx_gain(gain_db, rx_channel, "")
                        .with_context(|| format!("hot set_rx_gain(ch={rx_channel}) failed"))?;
                }
            }
            self.applied_center_hz = center_hz;
            self.applied_bandwidth_hz = bandwidth_hz;
            self.applied_gain_db = gain_db;
            self.applied_config_revision = config_revision;
            update_state(&self.shared, |s| {
                if s.config_revision == config_revision {
                    s.bandwidth_hz = bandwidth_hz;
                    s.applied_revision = config_revision;
                    s.status = "Streaming".to_string();
                    s.detail = format!(
                        "RX hot tuned: ch{} / {:.6} MHz / {:.3} MHz BW / {:.1} dB",
                        rx_channel,
                        center_hz / 1e6,
                        bandwidth_hz / 1e6,
                        gain_db
                    );
                }
            });
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
