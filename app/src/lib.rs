use anyhow::Context;
use eframe::{egui, NativeOptions, Renderer};
#[cfg(target_os = "android")]
use egui_winit::winit;
use log::{error, info};
use num_complex::Complex32;
use rustfft::FftPlanner;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant};

mod android_uhd_context;
mod usb;
mod uhd_wrapper;

const DEFAULT_FFT_SIZE: usize = 8192;
const WATERFALL_ROWS: usize = 96;
const SAFE_TOP_PAD: i8 = 72;
const SAFE_SIDE_PAD: i8 = 18;
const SAFE_BOTTOM_PAD: i8 = 18;

/// Global storage for the B210 USB file descriptor obtained in android_on_create.
static B210_FD: OnceLock<Mutex<Option<i32>>> = OnceLock::new();
static INIT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static SDR_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct SdrUiState {
    status: String,
    detail: String,
    device: String,
    center_hz: f64,
    bandwidth_hz: f64,
    sample_rate: f64,
    gain_db: f64,
    config_revision: u64,
    applied_revision: u64,
    opened: bool,
    streaming: bool,
    fft_size: usize,
    fft_fps: f32,
    fft_smoothing: f32,
    fft_revision: u64,
    applied_fft_revision: u64,
    spectrum_db: Vec<f32>,
    waterfall: Vec<Vec<f32>>,
    frames: u64,
    last_update: Instant,
}

impl Default for SdrUiState {
    fn default() -> Self {
        Self {
            status: "Waiting for B210".to_string(),
            detail: "Attach USRP B210 and grant USB permission".to_string(),
            device: "—".to_string(),
            center_hz: 100.0e6,
            bandwidth_hz: 1.0e6,
            sample_rate: 1.0e6,
            gain_db: 20.0,
            config_revision: 0,
            applied_revision: 0,
            opened: false,
            streaming: false,
            fft_size: DEFAULT_FFT_SIZE,
            fft_fps: 15.0,
            fft_smoothing: 0.65,
            fft_revision: 0,
            applied_fft_revision: 0,
            spectrum_db: vec![-100.0; DEFAULT_FFT_SIZE],
            waterfall: Vec::new(),
            frames: 0,
            last_update: Instant::now(),
        }
    }
}

type SharedState = Arc<Mutex<SdrUiState>>;

fn set_b210_fd(fd: i32) {
    B210_FD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .replace(fd);
}

fn take_b210_fd() -> Option<i32> {
    B210_FD.get().and_then(|m| m.lock().unwrap().take())
}

fn update_state(shared: &SharedState, f: impl FnOnce(&mut SdrUiState)) {
    if let Ok(mut state) = shared.lock() {
        f(&mut state);
    }
}

fn try_open_b210_once(vm: *mut std::ffi::c_void, activity: *mut std::ffi::c_void, shared: &SharedState) -> Option<uhd::Usrp> {
    if INIT_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return None;
    }

    let fd = take_b210_fd().or_else(|| usb::open_b210_usb(vm, activity));
    let result = if let Some(fd) = fd {
        update_state(shared, |s| {
            s.status = "Opening B210".to_string();
            s.detail = format!("Got Android USB fd={fd}; initializing UHD");
        });
        info!("Got fd={fd}, initializing libusb + UHD...");
        match uhd_wrapper::init_b210_with_fd(fd) {
            Ok(usrp) => {
                let name = usrp.get_motherboard_name(0).unwrap_or_default();
                let rx = usrp.get_num_rx_channels().unwrap_or(0);
                let tx = usrp.get_num_tx_channels().unwrap_or(0);
                info!("B210 OPENED: {name} (RX={rx}, TX={tx})");
                update_state(shared, |s| {
                    s.status = "B210 opened".to_string();
                    s.detail = format!("{name}: RX={rx}, TX={tx}");
                    s.device = name;
                    s.opened = true;
                });
                Some(usrp)
            }
            Err(e) => {
                info!("B210 init failed: {e}");
                update_state(shared, |s| {
                    s.status = "Waiting for B210".to_string();
                    s.detail = e.to_string();
                    s.opened = false;
                    s.streaming = false;
                });
                usb::close_current_usb_connection(vm);
                None
            }
        }
    } else {
        update_state(shared, |s| {
            s.status = "Waiting for B210".to_string();
            s.detail = "No B210 fd available".to_string();
        });
        None
    };

    INIT_IN_PROGRESS.store(false, Ordering::SeqCst);
    result
}

#[cfg(target_os = "android")]
fn start_sdr_worker(app: winit::platform::android::activity::AndroidApp, shared: SharedState) {
    if SDR_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    thread::spawn(move || loop {
        let vm = app.vm_as_ptr();
        let activity = app.activity_as_ptr();

        match try_open_b210_once(vm, activity, &shared) {
            Some(mut usrp) => {
                if let Err(e) = configure_and_stream(&mut usrp, &shared) {
                    error!("SDR stream stopped: {e:?}");
                    update_state(&shared, |s| {
                        s.status = "B210 opened; RX stream failed".to_string();
                        s.detail = format!("No live samples: {e}");
                        s.streaming = false;
                        s.spectrum_db.fill(-120.0);
                        s.waterfall.clear();
                    });
                    // Keep the USRP handle alive even if streaming setup failed.
                    loop {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            None => thread::sleep(Duration::from_millis(250)),
        }
    });
}

fn configure_and_stream(usrp: &mut uhd::Usrp, shared: &SharedState) -> anyhow::Result<()> {
    let _ = usrp.set_rx_antenna("RX2", 0);
    let _ = usrp.set_rx_dc_offset_enabled(true, 0);

    let mut planner = FftPlanner::<f32>::new();
    let (mut fft_size, mut publish_interval, mut smoothing, mut applied_fft_revision) = current_fft_config(shared);
    let mut fft = planner.plan_fft_forward(fft_size);
    let mut rx_buf = vec![Complex32::new(0.0, 0.0); fft_size];
    let mut fft_buf = vec![Complex32::new(0.0, 0.0); fft_size];
    let mut smoothed_spectrum = vec![-100.0; fft_size];
    let mut last_ui_publish = Instant::now() - publish_interval;
    update_state(shared, |s| s.applied_fft_revision = applied_fft_revision);

    loop {
        apply_rx_config(usrp, shared)?;

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

        info!("UHD RX stream active; computing {fft_size}-point FFT for UI");
        update_state(shared, |s| {
            s.status = "Streaming".to_string();
            s.detail = "UHD RX stream active".to_string();
            s.streaming = true;
        });

        loop {
            let needs_reconfig = shared
                .lock()
                .map(|s| s.config_revision != s.applied_revision)
                .unwrap_or(false);
            if needs_reconfig {
                info!("RX config changed; restarting UHD RX streamer");
                let _ = streamer.send_command(&uhd::StreamCommand {
                    time: uhd::StreamTime::Now,
                    command_type: uhd::StreamCommandType::StopContinuous,
                });
                update_state(shared, |s| {
                    s.streaming = false;
                    s.status = "Retuning".to_string();
                });
                break;
            }

            let (new_fft_size, new_interval, new_smoothing, new_fft_revision) = current_fft_config(shared);
            if new_fft_revision != applied_fft_revision {
                fft_size = new_fft_size;
                publish_interval = new_interval;
                smoothing = new_smoothing;
                applied_fft_revision = new_fft_revision;
                fft = planner.plan_fft_forward(fft_size);
                rx_buf.resize(fft_size, Complex32::new(0.0, 0.0));
                fft_buf.resize(fft_size, Complex32::new(0.0, 0.0));
                smoothed_spectrum.resize(fft_size, -100.0);
                update_state(shared, |s| {
                    s.applied_fft_revision = applied_fft_revision;
                    s.spectrum_db.resize(fft_size, -100.0);
                    s.waterfall.clear();
                    s.detail = format!("FFT: {fft_size} bins @ {:.1} FPS", 1.0 / publish_interval.as_secs_f32());
                });
                info!("FFT config applied: size={fft_size}, fps={:.1}, smoothing={smoothing:.2}", 1.0 / publish_interval.as_secs_f32());
            }

            let md = streamer.receive_simple(&mut rx_buf)?;
            let n = md.samples().min(fft_size);
            if n == 0 {
                continue;
            }
            // Do not push every UHD packet to the UI: at MHz sample rates that makes
            // the waterfall scroll too fast and wastes CPU.
            if last_ui_publish.elapsed() >= publish_interval {
                fft_buf.fill(Complex32::new(0.0, 0.0));
                fft_buf[..n].copy_from_slice(&rx_buf[..n]);
                apply_hann(&mut fft_buf[..n]);
                fft.process(&mut fft_buf);
                let spectrum = fft_to_db(&fft_buf);
                smooth_spectrum(&spectrum, &mut smoothed_spectrum, smoothing);
                update_spectrum(shared, smoothed_spectrum.clone());
                last_ui_publish = Instant::now();
                if shared.lock().map(|s| s.frames % 120 == 0).unwrap_or(false) {
                    info!("RX FFT frames: {}", shared.lock().map(|s| s.frames).unwrap_or(0));
                }
            }
        }
    }
}

fn apply_rx_config(usrp: &mut uhd::Usrp, shared: &SharedState) -> anyhow::Result<()> {
    let (center_hz, bandwidth_hz, gain_db, revision) = {
        let state = shared.lock().unwrap();
        (
            state.center_hz,
            state.bandwidth_hz,
            state.gain_db,
            state.config_revision,
        )
    };
    let sample_rate = bandwidth_hz;

    update_state(shared, |s| {
        s.status = "Configuring RX".to_string();
        s.detail = format!(
            "center={:.6} MHz, bandwidth={:.3} MHz, gain={:.1} dB",
            center_hz / 1e6,
            bandwidth_hz / 1e6,
            gain_db
        );
    });

    info!(
        "Applying RX config rev={revision}: center={:.6} MHz bandwidth={:.3} MHz gain={:.1} dB",
        center_hz / 1e6,
        bandwidth_hz / 1e6,
        gain_db
    );

    usrp
        .set_rx_sample_rate(sample_rate, 0)
        .context("set_rx_sample_rate failed")?;
    usrp
        .set_rx_bandwidth(bandwidth_hz, 0)
        .context("set_rx_bandwidth failed")?;
    usrp
        .set_rx_frequency(&uhd::TuneRequest::with_frequency(center_hz), 0)
        .context("set_rx_frequency failed")?;
    usrp
        .set_rx_gain(gain_db, 0, "")
        .context("set_rx_gain failed")?;

    update_state(shared, |s| {
        s.sample_rate = sample_rate;
        s.applied_revision = revision;
        s.status = if s.streaming { "Streaming" } else { "B210 opened" }.to_string();
        s.detail = format!(
            "RX tuned: {:.6} MHz / {:.3} MHz BW",
            center_hz / 1e6,
            bandwidth_hz / 1e6
        );
    });

    Ok(())
}

fn current_fft_config(shared: &SharedState) -> (usize, Duration, f32, u64) {
    let state = shared.lock().unwrap();
    let fft_size = state.fft_size.clamp(1024, 16384).next_power_of_two();
    let fps = state.fft_fps.clamp(5.0, 30.0);
    let interval = Duration::from_secs_f32(1.0 / fps);
    let smoothing = state.fft_smoothing.clamp(0.0, 0.95);
    (fft_size, interval, smoothing, state.fft_revision)
}

fn smooth_spectrum(input: &[f32], output: &mut Vec<f32>, smoothing: f32) {
    if output.len() != input.len() {
        *output = input.to_vec();
        return;
    }
    for (out, new) in output.iter_mut().zip(input.iter()) {
        *out = *out * smoothing + *new * (1.0 - smoothing);
    }
}

fn apply_hann(samples: &mut [Complex32]) {
    let n = samples.len().max(1) as f32;
    for (i, sample) in samples.iter_mut().enumerate() {
        let w = 0.5 - 0.5 * ((2.0 * std::f32::consts::PI * i as f32) / n).cos();
        *sample *= w;
    }
}

fn fft_to_db(fft: &[Complex32]) -> Vec<f32> {
    let n = fft.len();
    (0..n)
        .map(|i| {
            let shifted = (i + n / 2) % n;
            let p = fft[shifted].norm_sqr() / n as f32;
            (10.0 * p.max(1.0e-12).log10()).clamp(-120.0, 10.0)
        })
        .collect()
}

fn update_spectrum(shared: &SharedState, spectrum: Vec<f32>) {
    update_state(shared, |s| {
        s.spectrum_db = spectrum.clone();
        s.waterfall.push(spectrum);
        if s.waterfall.len() > WATERFALL_ROWS {
            let extra = s.waterfall.len() - WATERFALL_ROWS;
            s.waterfall.drain(0..extra);
        }
        s.frames += 1;
        s.last_update = Instant::now();
    });
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    std::env::set_var("RUST_BACKTRACE", "full");
    std::env::set_var("WGPU_BACKEND", "gles");

    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("pixdr"),
        );
    });

    info!("pixdr egui started");
    let shared = Arc::new(Mutex::new(SdrUiState::default()));
    start_sdr_worker(app.clone(), shared.clone());

    let options = NativeOptions {
        android_app: Some(app),
        renderer: Renderer::Wgpu,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "pixdr",
        options,
        Box::new(move |_cc| Ok(Box::new(PixdrApp::new(shared)))),
    ) {
        error!("eframe failed: {e:?}");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UiTab {
    Radio,
    Fft,
}

struct PixdrApp {
    shared: SharedState,
    active_tab: UiTab,
}

impl PixdrApp {
    fn new(shared: SharedState) -> Self {
        Self {
            shared,
            active_tab: UiTab::Radio,
        }
    }

}

impl eframe::App for PixdrApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(33));

        let state = self.shared.lock().unwrap().clone();
        ui.visuals_mut().dark_mode = true;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(8, 10, 14))
                    // Pixel punch-hole / rounded-corner conservative padding.
                    // egui content_rect is not enough on this NativeActivity path.
                    .inner_margin(egui::Margin {
                        left: SAFE_SIDE_PAD,
                        right: SAFE_SIDE_PAD,
                        top: SAFE_TOP_PAD,
                        bottom: SAFE_BOTTOM_PAD,
                    }),
            )
            .show_inside(ui, |ui| {
                ui.set_clip_rect(ui.max_rect());
                draw_header(ui, &state);
                ui.add_space(4.0);
                draw_tabs(ui, &mut self.active_tab);
                ui.add_space(4.0);
                match self.active_tab {
                    UiTab::Radio => draw_controls(ui, &self.shared, &state),
                    UiTab::Fft => draw_fft_controls(ui, &self.shared, &state),
                }
                ui.add_space(6.0);
                draw_spectrum(ui, &state);
                ui.add_space(6.0);
                draw_waterfall(ui, &state);
                ui.add_space(4.0);
                draw_status(ui, &state);
            });
    }
}

fn draw_header(ui: &mut egui::Ui, state: &SdrUiState) {
    ui.horizontal(|ui| {
        ui.heading("pixdr");
        ui.separator();
        let color = if state.streaming {
            egui::Color32::LIGHT_GREEN
        } else if state.opened {
            egui::Color32::YELLOW
        } else {
            egui::Color32::LIGHT_RED
        };
        ui.colored_label(color, &state.status);
    });
}

fn draw_tabs(ui: &mut egui::Ui, active: &mut UiTab) {
    ui.horizontal(|ui| {
        if ui.selectable_label(*active == UiTab::Radio, "Radio").clicked() {
            *active = UiTab::Radio;
        }
        if ui.selectable_label(*active == UiTab::Fft, "FFT").clicked() {
            *active = UiTab::Fft;
        }
    });
}

fn draw_controls(ui: &mut egui::Ui, shared: &SharedState, state: &SdrUiState) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(20, 22, 26))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Device: {}", state.device));
                ui.separator();
                ui.label(if state.streaming { "RX: live UHD" } else { "RX: preview" });
                ui.separator();
                ui.label(format!("rev {}/{}", state.applied_revision, state.config_revision));
            });
            ui.separator();
            draw_tuning_controls(ui, shared, state);
        });
}

fn draw_tuning_controls(ui: &mut egui::Ui, shared: &SharedState, state: &SdrUiState) {
    let mut center_mhz = state.center_hz / 1e6;
    let mut bandwidth_mhz = state.bandwidth_hz / 1e6;
    let mut gain_db = state.gain_db;
    let mut changed = false;

    ui.label(format!("Center frequency: {:.6} MHz", center_mhz));
    ui.horizontal(|ui| {
        changed |= ui.add_sized([62.0, 30.0], egui::Button::new("−10M")).clicked().then(|| center_mhz -= 10.0).is_some();
        changed |= ui.add_sized([62.0, 30.0], egui::Button::new("−1M")).clicked().then(|| center_mhz -= 1.0).is_some();
        changed |= ui.add_sized([72.0, 30.0], egui::Button::new("−100k")).clicked().then(|| center_mhz -= 0.1).is_some();
    });
    let resp = ui.add_sized(
        [ui.available_width(), 30.0],
        egui::Slider::new(&mut center_mhz, 50.0..=6000.0)
            .text("MHz")
            .step_by(0.001),
    );
    changed |= resp.changed();
    ui.horizontal(|ui| {
        changed |= ui.add_sized([72.0, 30.0], egui::Button::new("+100k")).clicked().then(|| center_mhz += 0.1).is_some();
        changed |= ui.add_sized([62.0, 30.0], egui::Button::new("+1M")).clicked().then(|| center_mhz += 1.0).is_some();
        changed |= ui.add_sized([62.0, 30.0], egui::Button::new("+10M")).clicked().then(|| center_mhz += 10.0).is_some();
    });

    ui.add_space(3.0);
    ui.label(format!("Bandwidth / sample rate: {:.3} MHz", bandwidth_mhz));
    ui.horizontal_wrapped(|ui| {
        for bw in [0.2, 0.5, 1.0, 2.0, 5.0, 10.0] {
            if ui.add_sized([58.0, 28.0], egui::Button::new(format!("{bw:.1}M"))).clicked() {
                bandwidth_mhz = bw;
                changed = true;
            }
        }
    });
    let resp = ui.add_sized(
        [ui.available_width(), 30.0],
        egui::Slider::new(&mut bandwidth_mhz, 0.2..=20.0)
            .text("MHz")
            .step_by(0.1),
    );
    changed |= resp.changed();

    ui.add_space(3.0);
    ui.label(format!("Gain: {:.1} dB", gain_db));
    let resp = ui.add_sized(
        [ui.available_width(), 30.0],
        egui::Slider::new(&mut gain_db, 0.0..=76.0)
            .text("dB")
            .step_by(1.0),
    );
    changed |= resp.changed();

    if changed {
        center_mhz = center_mhz.clamp(50.0, 6000.0);
        bandwidth_mhz = bandwidth_mhz.clamp(0.2, 20.0);
        gain_db = gain_db.clamp(0.0, 76.0);
        update_state(shared, |s| {
            s.center_hz = center_mhz * 1e6;
            s.bandwidth_hz = bandwidth_mhz * 1e6;
            s.sample_rate = s.bandwidth_hz;
            s.gain_db = gain_db;
            s.config_revision = s.config_revision.wrapping_add(1);
            s.detail = format!(
                "Pending tune: {:.6} MHz / {:.3} MHz BW / {:.1} dB",
                center_mhz, bandwidth_mhz, gain_db
            );
        });
    }
}

fn draw_fft_controls(ui: &mut egui::Ui, shared: &SharedState, state: &SdrUiState) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(20, 22, 26))
        .show(ui, |ui| {
            let mut fft_size = state.fft_size;
            let mut fft_fps = state.fft_fps;
            let mut smoothing = state.fft_smoothing;
            let mut changed = false;

            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "FFT applied: {} bins, {:.1} FPS, smoothing {:.2}",
                    state.fft_size, state.fft_fps, state.fft_smoothing
                ));
                ui.separator();
                ui.label(format!("rev {}/{}", state.applied_fft_revision, state.fft_revision));
            });
            ui.separator();

            ui.label("FFT size");
            ui.horizontal_wrapped(|ui| {
                for size in [1024usize, 2048, 4096, 8192, 16384] {
                    if ui
                        .selectable_label(fft_size == size, format!("{size}"))
                        .clicked()
                    {
                        fft_size = size;
                        changed = true;
                    }
                }
            });

            ui.add_space(4.0);
            ui.label(format!("FFT / waterfall FPS: {:.1}", fft_fps));
            changed |= ui
                .add_sized(
                    [ui.available_width(), 30.0],
                    egui::Slider::new(&mut fft_fps, 5.0..=30.0)
                        .text("FPS")
                        .step_by(1.0),
                )
                .changed();

            ui.add_space(4.0);
            ui.label(format!("Spectrum smoothing: {:.2}", smoothing));
            changed |= ui
                .add_sized(
                    [ui.available_width(), 30.0],
                    egui::Slider::new(&mut smoothing, 0.0..=0.95)
                        .text("EMA")
                        .step_by(0.05),
                )
                .changed();

            ui.horizontal_wrapped(|ui| {
                ui.small("Tip: larger FFT = sharper frequency resolution; lower FPS = slower waterfall; higher smoothing = less noisy but more lag.");
            });

            if changed {
                update_state(shared, |s| {
                    s.fft_size = fft_size.clamp(1024, 16384).next_power_of_two();
                    s.fft_fps = fft_fps.clamp(5.0, 30.0);
                    s.fft_smoothing = smoothing.clamp(0.0, 0.95);
                    s.fft_revision = s.fft_revision.wrapping_add(1);
                    s.detail = format!(
                        "Pending FFT: {} bins @ {:.1} FPS, smoothing {:.2}",
                        s.fft_size, s.fft_fps, s.fft_smoothing
                    );
                });
            }
        });
}

fn draw_spectrum(ui: &mut egui::Ui, state: &SdrUiState) {
    let desired = egui::vec2(ui.available_width(), (ui.available_height() * 0.48).clamp(260.0, 520.0));
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(5, 7, 10));

    draw_grid(&painter, rect);

    if state.streaming && state.frames > 0 {
        let min_db = -120.0;
        let max_db = 0.0;
        let points: Vec<egui::Pos2> = state
            .spectrum_db
            .iter()
            .enumerate()
            .map(|(i, db)| {
                let x = rect.left() + rect.width() * i as f32 / (state.spectrum_db.len().saturating_sub(1).max(1)) as f32;
                let t = ((*db - min_db) / (max_db - min_db)).clamp(0.0, 1.0);
                let y = rect.bottom() - rect.height() * t;
                egui::pos2(x, y)
            })
            .collect();

        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.6, egui::Color32::from_rgb(90, 220, 120)),
        ));
    } else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No live RX stream\nB210 is open, but UHD streaming is not active",
            egui::FontId::proportional(18.0),
            egui::Color32::LIGHT_RED,
        );
    }
    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        "Spectrum (dBFS)",
        egui::FontId::proportional(15.0),
        egui::Color32::LIGHT_GRAY,
    );
    painter.text(
        rect.right_top() + egui::vec2(-8.0, 8.0),
        egui::Align2::RIGHT_TOP,
        format!(
            "{:.6} MHz  span {:.3} MHz",
            state.center_hz / 1e6,
            state.bandwidth_hz / 1e6
        ),
        egui::FontId::proportional(15.0),
        egui::Color32::LIGHT_GRAY,
    );
    painter.text(
        rect.left_bottom() + egui::vec2(8.0, -8.0),
        egui::Align2::LEFT_BOTTOM,
        format!("{:.6} MHz", (state.center_hz - state.bandwidth_hz / 2.0) / 1e6),
        egui::FontId::proportional(13.0),
        egui::Color32::GRAY,
    );
    painter.text(
        rect.right_bottom() + egui::vec2(-8.0, -8.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{:.6} MHz", (state.center_hz + state.bandwidth_hz / 2.0) / 1e6),
        egui::FontId::proportional(13.0),
        egui::Color32::GRAY,
    );
}

fn draw_grid(painter: &egui::Painter, rect: egui::Rect) {
    let grid = egui::Color32::from_gray(45);
    for i in 1..10 {
        let x = rect.left() + rect.width() * i as f32 / 10.0;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(0.6, grid),
        );
    }
    for i in 1..6 {
        let y = rect.top() + rect.height() * i as f32 / 6.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.6, grid),
        );
    }
}

fn draw_waterfall(ui: &mut egui::Ui, state: &SdrUiState) {
    let desired = egui::vec2(ui.available_width(), (ui.available_height() - 34.0).clamp(180.0, 520.0));
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::BLACK);

    let rows = state.waterfall.len().max(1);
    let bins = ((rect.width() / 3.0).round() as usize)
        .clamp(160, 768)
        .min(state.spectrum_db.len().max(1));
    let row_h = rect.height() / WATERFALL_ROWS as f32;
    let bin_w = rect.width() / bins as f32;

    if state.streaming && !state.waterfall.is_empty() {
        for (r, spectrum) in state.waterfall.iter().rev().enumerate() {
            let y0 = rect.bottom() - (r as f32 + 1.0) * row_h;
            if y0 < rect.top() {
                break;
            }
            for b in 0..bins {
                let idx = b * spectrum.len() / bins;
                let c = waterfall_color(spectrum[idx]);
                let x0 = rect.left() + b as f32 * bin_w;
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(bin_w + 1.0, row_h + 1.0)),
                    0.0,
                    c,
                );
            }
        }
    } else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Waterfall paused\nwaiting for live samples",
            egui::FontId::proportional(17.0),
            egui::Color32::GRAY,
        );
    }

    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        format!("Waterfall — {} frames", state.frames.max(rows as u64)),
        egui::FontId::proportional(15.0),
        egui::Color32::WHITE,
    );
}

fn waterfall_color(db: f32) -> egui::Color32 {
    let t = ((db + 110.0) / 80.0).clamp(0.0, 1.0);
    let r = (255.0 * (t - 0.45).max(0.0) / 0.55).clamp(0.0, 255.0) as u8;
    let g = (255.0 * (1.0 - (t - 0.55).abs() / 0.55).clamp(0.0, 1.0)) as u8;
    let b = (255.0 * (1.0 - t).powf(1.5)).clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgb(r, g, b)
}

fn draw_status(ui: &mut egui::Ui, state: &SdrUiState) {
    ui.horizontal_wrapped(|ui| {
        ui.label(&state.detail);
        ui.separator();
        ui.label(format!("last update: {:.1}s ago", state.last_update.elapsed().as_secs_f32()));
    });
}

/// USB permission + fd acquisition on Java main thread.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_on_create(state: &android_activity::OnCreateState) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("pixdr"),
        );
    });

    info!("android_on_create: opening B210 USB...");
    match usb::open_b210_usb(state.vm_as_ptr(), state.activity_as_ptr()) {
        Some(fd) => {
            info!("B210 USB fd={fd}");
            set_b210_fd(fd);
        }
        None => info!("B210 not available"),
    }
}
