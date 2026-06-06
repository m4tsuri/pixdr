use anyhow::Context;
use eframe::{egui, NativeOptions, Renderer};
#[cfg(target_os = "android")]
use egui_winit::winit;
use log::{error, info};
use num_complex::Complex32;
use std::io::Read;
#[cfg(target_os = "android")]
use std::os::fd::FromRawFd;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant};

mod android_uhd_context;
mod futuresdr_backend;
#[cfg(target_os = "android")]
mod file_picker;
mod usb;
mod uhd_wrapper;

const DEFAULT_FFT_SIZE: usize = 8192;
const WATERFALL_ROWS: usize = 96;
const MAX_CONSTELLATION_POINTS: usize = 2048;
const SAFE_TOP_PAD: i8 = 72;
const SAFE_SIDE_PAD: i8 = 18;
const SAFE_BOTTOM_PAD: i8 = 18;
const SPECTRUM_GESTURE_DEBOUNCE: Duration = Duration::from_millis(250);
const TX_IMAGE_PICKER_REQUEST: i32 = 4201;
const TX_IMAGE_WIDTH: u32 = 256;
const TX_IMAGE_HEIGHT: u32 = 96;

/// Global storage for the B210 USB file descriptor obtained in android_on_create.
static B210_FD: OnceLock<Mutex<Option<i32>>> = OnceLock::new();
static INIT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static SDR_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static APP_SHARED: OnceLock<SharedState> = OnceLock::new();
#[cfg(target_os = "android")]
static ANDROID_VM_PTR: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_os = "android")]
static ANDROID_ACTIVITY_PTR: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct SdrUiState {
    status: String,
    detail: String,
    device: String,
    center_hz: f64,
    bandwidth_hz: f64,
    sample_rate: f64,
    gain_db: f64,
    rx_channel: usize,
    rx_channels: usize,
    tx_gain_db: f64,
    tx_channel: usize,
    tx_channels: usize,
    tx_image_uri: String,
    tx_image_name: String,
    tx_image_gray: Vec<u8>,
    tx_image_width: usize,
    tx_image_height: usize,
    tx_center_hz: f64,
    tx_lo_offset_hz: f64,
    tx_sample_rate: f64,
    tx_row_ms: f64,
    tx_amplitude: f64,
    tx_loop: bool,
    tx_enabled: bool,
    tx_transmitting: bool,
    tx_revision: u64,
    tx_applied_revision: u64,
    config_revision: u64,
    applied_revision: u64,
    graph_revision: u64,
    applied_graph_revision: u64,
    opened: bool,
    streaming: bool,
    user_paused: bool,
    user_stopped: bool,
    fft_size: usize,
    fft_fps: f32,
    fft_smoothing: f32,
    fft_revision: u64,
    applied_fft_revision: u64,
    spectrum_db: Vec<f32>,
    waterfall: Vec<Vec<f32>>,
    constellation: Vec<Complex32>,
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
            rx_channel: 0,
            rx_channels: 0,
            tx_gain_db: 0.0,
            tx_channel: 0,
            tx_channels: 0,
            tx_image_uri: String::new(),
            tx_image_name: String::new(),
            tx_image_gray: Vec::new(),
            tx_image_width: 0,
            tx_image_height: 0,
            tx_center_hz: 100.0e6,
            tx_lo_offset_hz: 250.0e3,
            tx_sample_rate: 1.0e6,
            tx_row_ms: 35.0,
            tx_amplitude: 0.85,
            tx_loop: false,
            tx_enabled: false,
            tx_transmitting: false,
            tx_revision: 0,
            tx_applied_revision: 0,
            config_revision: 0,
            applied_revision: 0,
            graph_revision: 0,
            applied_graph_revision: 0,
            opened: false,
            streaming: false,
            user_paused: false,
            user_stopped: false,
            fft_size: DEFAULT_FFT_SIZE,
            fft_fps: 15.0,
            fft_smoothing: 0.65,
            fft_revision: 0,
            applied_fft_revision: 0,
            spectrum_db: vec![-100.0; DEFAULT_FFT_SIZE],
            waterfall: Vec::new(),
            constellation: Vec::new(),
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
                    s.rx_channels = rx;
                    s.tx_channels = tx;
                    if rx == 0 {
                        s.rx_channel = 0;
                    } else if s.rx_channel >= rx {
                        s.rx_channel = rx - 1;
                    }
                    if tx == 0 {
                        s.tx_channel = 0;
                    } else if s.tx_channel >= tx {
                        s.tx_channel = tx - 1;
                    }
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

        if shared.lock().map(|s| s.user_stopped).unwrap_or(false) {
            thread::sleep(Duration::from_millis(250));
            continue;
        }

        match try_open_b210_once(vm, activity, &shared) {
            Some(usrp) => {
                let usrp = Arc::new(Mutex::new(usrp));
                loop {
                    if shared.lock().map(|s| s.user_stopped).unwrap_or(false) {
                        drop(usrp);
                        usb::close_current_usb_connection(vm);
                        update_state(&shared, |s| {
                            s.streaming = false;
                            s.opened = false;
                            s.status = "Stopped".to_string();
                            s.detail = "RX stopped; UHD handle and Android USB connection closed".to_string();
                        });
                        break;
                    }
                    if shared.lock().map(|s| s.user_paused).unwrap_or(false) {
                        update_state(&shared, |s| {
                            s.streaming = false;
                            s.status = "Paused".to_string();
                        });
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    match futuresdr_backend::run_b210_flowgraph(usrp.clone(), shared.clone()) {
                        Ok(()) => {
                            if shared.lock().map(|s| s.user_stopped).unwrap_or(false) {
                                continue;
                            }
                        }
                        Err(e) => {
                            error!("FutureSDR graph stopped: {e:?}");
                            update_state(&shared, |s| {
                                s.status = "B210 opened; SDR graph failed".to_string();
                                s.detail = format!("No live samples: {e}");
                                s.streaming = false;
                                s.tx_transmitting = false;
                                // Keep the last spectrum/waterfall frame visible. Retunes and transient
                                // stream setup failures should look frozen, not like a hard reset.
                            });
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            }
            None => thread::sleep(Duration::from_millis(250)),
        }
    });
}

fn apply_rx_config(usrp: &mut uhd::Usrp, shared: &SharedState) -> anyhow::Result<()> {
    let (center_hz, mut bandwidth_hz, sample_rate, gain_db, rx_channel, revision) = {
        let state = shared.lock().unwrap();
        (
            state.center_hz,
            state.bandwidth_hz,
            state.sample_rate,
            state.gain_db,
            state.rx_channel,
            state.config_revision,
        )
    };
    bandwidth_hz = bandwidth_hz.min(sample_rate);

    update_state(shared, |s| {
        s.status = "Configuring RX".to_string();
        s.detail = format!(
            "ch={}, center={:.6} MHz, bandwidth={:.3} MHz, rate={:.3} MS/s, gain={:.1} dB",
            rx_channel,
            center_hz / 1e6,
            bandwidth_hz / 1e6,
            sample_rate / 1e6,
            gain_db
        );
    });

    info!(
        "Applying RX config rev={revision}: ch={} center={:.6} MHz bandwidth={:.3} MHz sample_rate={:.3} MS/s gain={:.1} dB",
        rx_channel,
        center_hz / 1e6,
        bandwidth_hz / 1e6,
        sample_rate / 1e6,
        gain_db
    );

    usrp
        .set_rx_sample_rate(sample_rate, rx_channel)
        .with_context(|| format!("set_rx_sample_rate(ch={rx_channel}) failed"))?;
    usrp
        .set_rx_bandwidth(bandwidth_hz, rx_channel)
        .with_context(|| format!("set_rx_bandwidth(ch={rx_channel}) failed"))?;
    usrp
        .set_rx_frequency(&uhd::TuneRequest::with_frequency(center_hz), rx_channel)
        .with_context(|| format!("set_rx_frequency(ch={rx_channel}) failed"))?;
    usrp
        .set_rx_gain(gain_db, rx_channel, "")
        .with_context(|| format!("set_rx_gain(ch={rx_channel}) failed"))?;

    update_state(shared, |s| {
        if s.config_revision == revision {
            // Only acknowledge the config if the UI has not moved on while UHD
            // was applying it. Otherwise, do not write this stale snapshot back
            // into state; the FutureSDR source will immediately stop and the
            // next graph will apply the newest revision.
            s.bandwidth_hz = bandwidth_hz;
            s.sample_rate = sample_rate;
            s.rx_channel = rx_channel;
            s.applied_revision = revision;
            s.status = if s.streaming { "Streaming" } else { "B210 opened" }.to_string();
            s.detail = format!(
                "RX tuned: ch{} / {:.6} MHz / {:.3} MHz BW / {:.3} MS/s",
                rx_channel,
                center_hz / 1e6,
                bandwidth_hz / 1e6,
                sample_rate / 1e6
            );
        }
    });

    Ok(())
}

fn apply_tx_config(usrp: &mut uhd::Usrp, shared: &SharedState) -> anyhow::Result<u64> {
    let (center_hz, sample_rate, gain_db, tx_channel, lo_offset_hz, revision) = {
        let state = shared.lock().unwrap();
        let sample_rate = state.tx_sample_rate;
        let lo_limit = sample_rate * 0.45;
        (
            state.tx_center_hz,
            sample_rate,
            state.tx_gain_db,
            state.tx_channel,
            state.tx_lo_offset_hz.clamp(-lo_limit, lo_limit),
            state.tx_revision,
        )
    };

    update_state(shared, |s| {
        if s.tx_enabled {
            s.status = "Configuring TX".to_string();
            s.detail = format!(
                "ch={}, center={:.6} MHz, LO offset={:+.0} kHz, rate={:.3} MS/s, gain={:.1} dB",
                tx_channel,
                center_hz / 1e6,
                lo_offset_hz / 1e3,
                sample_rate / 1e6,
                gain_db
            );
        }
    });

    info!(
        "Applying TX config rev={revision}: ch={} center={:.6} MHz lo_offset={:+.0} kHz rf_lo={:.6} MHz sample_rate={:.3} MS/s gain={:.1} dB",
        tx_channel,
        center_hz / 1e6,
        lo_offset_hz / 1e3,
        (center_hz + lo_offset_hz) / 1e6,
        sample_rate / 1e6,
        gain_db
    );

    usrp
        .set_tx_sample_rate(sample_rate, tx_channel)
        .with_context(|| format!("set_tx_sample_rate(ch={tx_channel}) failed"))?;
    let tune_request = if lo_offset_hz.abs() > 1.0 {
        // UHD-provided LO-offset tune request: keep the requested signal centered
        // at `center_hz`, while moving the analog TX LO to center+offset.
        uhd::TuneRequest::with_frequency_lo(center_hz, lo_offset_hz)
    } else {
        uhd::TuneRequest::with_frequency(center_hz)
    };
    let tune_result = usrp
        .set_tx_frequency(&tune_request, tx_channel)
        .with_context(|| format!("set_tx_frequency(ch={tx_channel}) failed"))?;
    info!(
        "TX tune result rev={revision}: target_rf={:.6} MHz actual_rf={:.6} MHz target_dsp={:+.0} Hz actual_dsp={:+.0} Hz",
        tune_result.target_rf_freq() / 1e6,
        tune_result.actual_rf_freq() / 1e6,
        tune_result.target_dsp_freq(),
        tune_result.actual_dsp_freq()
    );
    usrp
        .set_tx_gain(gain_db, tx_channel, "")
        .with_context(|| format!("set_tx_gain(ch={tx_channel}) failed"))?;

    update_state(shared, |s| {
        if s.tx_revision == revision {
            s.tx_applied_revision = revision;
            if s.tx_enabled {
                s.detail = format!(
                    "TX image: ch{} / {:.6} MHz / LO {:+.0} kHz / {:.3} MS/s",
                    tx_channel,
                    center_hz / 1e6,
                    lo_offset_hz / 1e3,
                    sample_rate / 1e6
                );
            }
        }
    });

    Ok(revision)
}

fn mute_tx_output(usrp: &mut uhd::Usrp, shared: &SharedState) -> anyhow::Result<()> {
    let tx_channel = shared.lock().map(|s| s.tx_channel).unwrap_or(0);
    usrp
        .set_tx_gain(0.0, tx_channel, "")
        .with_context(|| format!("mute set_tx_gain(ch={tx_channel}) failed"))?;
    info!("Muted TX output on ch{tx_channel} after burst");
    Ok(())
}

fn tx_image_snapshot(shared: &SharedState) -> anyhow::Result<TxImageSnapshot> {
    let state = shared.lock().unwrap();
    if state.tx_image_gray.is_empty() || state.tx_image_width == 0 || state.tx_image_height == 0 {
        anyhow::bail!("No TX image selected");
    }
    Ok(TxImageSnapshot {
        gray: state.tx_image_gray.clone(),
        width: state.tx_image_width,
        height: state.tx_image_height,
        sample_rate: state.tx_sample_rate,
        row_ms: state.tx_row_ms,
        amplitude: state.tx_amplitude,
        lo_offset_hz: state.tx_lo_offset_hz,
        loop_enabled: state.tx_loop,
        revision: state.tx_revision,
    })
}

#[derive(Clone)]
struct TxImageSnapshot {
    gray: Vec<u8>,
    width: usize,
    height: usize,
    sample_rate: f64,
    row_ms: f64,
    amplitude: f64,
    lo_offset_hz: f64,
    loop_enabled: bool,
    revision: u64,
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

fn update_signal_products(shared: &SharedState, spectrum: Vec<f32>, constellation: Vec<Complex32>) {
    update_state(shared, |s| {
        s.spectrum_db = spectrum.clone();
        s.waterfall.push(spectrum);
        s.constellation = constellation;
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
    ANDROID_VM_PTR.store(app.vm_as_ptr() as usize, Ordering::SeqCst);
    ANDROID_ACTIVITY_PTR.store(app.activity_as_ptr() as usize, Ordering::SeqCst);

    let shared = Arc::new(Mutex::new(SdrUiState::default()));
    let _ = APP_SHARED.set(shared.clone());
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

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_pixdr_app_PixdrActivity_nativeOnFilePicked(
    mut env: jni::JNIEnv<'_>,
    _activity: jni::sys::jobject,
    request_code: jni::sys::jint,
    uri: jni::sys::jstring,
    display_name: jni::sys::jstring,
    mime_type: jni::sys::jstring,
    fd: jni::sys::jint,
) {
    let _ = env
        .with_env(|env| -> jni::errors::Result<()> {
            let uri = unsafe { jni::objects::JString::from_raw(env, uri) };
            let display_name = unsafe { jni::objects::JString::from_raw(env, display_name) };
            let mime_type = unsafe { jni::objects::JString::from_raw(env, mime_type) };
            let uri: String = env.get_string(&uri)?.into();
            let display_name: String = env.get_string(&display_name)?.into();
            let mime_type: String = env.get_string(&mime_type)?.into();
            let label = if display_name.trim().is_empty() {
                "selected file".to_string()
            } else {
                display_name
            };
            info!("File picked request={request_code}: {label} ({mime_type}) fd={fd} {uri}");
            if request_code == TX_IMAGE_PICKER_REQUEST {
                if let Some(shared) = APP_SHARED.get() {
                    match load_tx_image_from_fd(fd, &uri, &label) {
                        Ok((gray, width, height)) => update_state(shared, |s| {
                            s.tx_image_uri = uri.clone();
                            s.tx_image_name = label.clone();
                            s.tx_image_gray = gray;
                            s.tx_image_width = width;
                            s.tx_image_height = height;
                            s.tx_revision = s.tx_revision.wrapping_add(1);
                            s.detail = format!("TX image selected: {label} ({width}x{height})");
                        }),
                        Err(e) => update_state(shared, |s| {
                            s.detail = format!("Could not load TX image: {e}");
                        }),
                    }
                }
            }
            Ok(())
        })
        .into_outcome();
}

#[cfg(target_os = "android")]
fn load_tx_image_from_fd(fd: i32, uri: &str, label: &str) -> anyhow::Result<(Vec<u8>, usize, usize)> {
    if fd < 0 {
        anyhow::bail!("Android picker did not provide a readable fd");
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading selected image {label} from {uri}"))?;
    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("decoding selected image {label}"))?;
    let gray = image
        .resize_exact(
            TX_IMAGE_WIDTH,
            TX_IMAGE_HEIGHT,
            image::imageops::FilterType::Triangle,
        )
        .to_luma8();
    Ok((gray.into_raw(), TX_IMAGE_WIDTH as usize, TX_IMAGE_HEIGHT as usize))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IoMode {
    Read,
    Write,
}

impl IoMode {
    fn label(self) -> &'static str {
        match self {
            IoMode::Read => "RX",
            IoMode::Write => "TX",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UiTab {
    Radio,
    Fft,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualView {
    Spectrum,
    Constellation,
}

impl VisualView {
    fn label(self) -> &'static str {
        match self {
            VisualView::Spectrum => "Spectrum",
            VisualView::Constellation => "Constellation",
        }
    }
}

struct PixdrApp {
    shared: SharedState,
    io_mode: IoMode,
    active_tab: UiTab,
    active_visual: VisualView,
    freq_edit_digit: Option<usize>,
    freq_edit_text: String,
    spectrum_pinch_active: bool,
    spectrum_last_commit: Option<Instant>,
}

impl PixdrApp {
    fn new(shared: SharedState) -> Self {
        Self {
            shared,
            io_mode: IoMode::Read,
            active_tab: UiTab::Radio,
            active_visual: VisualView::Spectrum,
            freq_edit_digit: None,
            freq_edit_text: String::new(),
            spectrum_pinch_active: false,
            spectrum_last_commit: None,
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
                draw_header(ui, &self.shared, &state, &mut self.io_mode);
                ui.add_space(4.0);
                match self.io_mode {
                    IoMode::Read => {
                        draw_tabs(ui, &mut self.active_tab);
                        ui.add_space(4.0);
                        match self.active_tab {
                            UiTab::Radio => draw_controls(
                                ui,
                                &self.shared,
                                &state,
                                &mut self.freq_edit_digit,
                                &mut self.freq_edit_text,
                            ),
                            UiTab::Fft => draw_fft_controls(ui, &self.shared, &state),
                        }
                        ui.add_space(6.0);
                        draw_visual_tabs(ui, &mut self.active_visual);
                        ui.add_space(4.0);
                        draw_visualization(
                            ui,
                            &self.shared,
                            &state,
                            self.active_visual,
                            &mut self.spectrum_pinch_active,
                            &mut self.spectrum_last_commit,
                        );
                    }
                    IoMode::Write => draw_write_mode(
                        ui,
                        &self.shared,
                        &state,
                        &mut self.freq_edit_digit,
                        &mut self.freq_edit_text,
                    ),
                }
                ui.add_space(4.0);
                draw_status(ui, &state);
            });
    }
}

fn draw_header(ui: &mut egui::Ui, shared: &SharedState, state: &SdrUiState, io_mode: &mut IoMode) {
    let header_height = 34.0;
    let width = ui.available_width().max(0.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, header_height), egui::Sense::hover());
    let y = rect.center().y;

    let brand_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 50.0, y - 2.0),
        egui::vec2(100.0, 30.0),
    );
    draw_brand_text(ui, brand_rect);

    let mut x = brand_rect.right() + 10.0;
    let sep_x = x;
    ui.painter().line_segment(
        [egui::pos2(sep_x, y - 12.0), egui::pos2(sep_x, y + 12.0)],
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(52, 60, 72)),
    );
    x += 10.0;

    for mode in [IoMode::Read, IoMode::Write] {
        let button_rect = egui::Rect::from_min_size(egui::pos2(x, y - 15.0), egui::vec2(58.0, 30.0));
        if draw_io_mode_button(ui, button_rect, mode, *io_mode == mode).clicked() {
            *io_mode = mode;
        }
        x += 61.0;
    }

    let color = if state.streaming || state.tx_transmitting {
        egui::Color32::LIGHT_GREEN
    } else if state.user_paused || state.opened {
        egui::Color32::YELLOW
    } else {
        egui::Color32::LIGHT_RED
    };

    let stop_rect = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 70.0, y - 15.0),
        egui::vec2(70.0, 30.0),
    );
    let status_rect = egui::Rect::from_min_size(
        egui::pos2(stop_rect.left() - 38.0, y - 15.0),
        egui::vec2(30.0, 30.0),
    );
    draw_header_status_ring_at(ui, shared, state, color, status_rect);
    draw_header_start_stop_at(ui, shared, state, stop_rect);
}

fn draw_brand_text(ui: &mut egui::Ui, rect: egui::Rect) {
    let font = egui::FontId::proportional(31.0);
    let color = egui::Color32::from_rgb(240, 244, 252);
    // egui painter text has no weight parameter; draw twice with a tiny offset
    // to give the pixdr brand a visibly heavier mark.
    ui.painter().text(
        rect.center() + egui::vec2(0.45, 0.0),
        egui::Align2::CENTER_CENTER,
        "pixdr",
        font.clone(),
        color,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "pixdr",
        font,
        color,
    );
}

fn draw_io_mode_button(ui: &mut egui::Ui, rect: egui::Rect, mode: IoMode, selected: bool) -> egui::Response {
    let text = egui::RichText::new(mode.label())
        .size(15.0)
        .strong()
        .color(if selected {
            egui::Color32::from_rgb(240, 246, 255)
        } else {
            egui::Color32::from_rgb(145, 155, 170)
        });
    let button = egui::Button::new(text)
        .fill(if selected {
            egui::Color32::from_rgb(24, 36, 52)
        } else {
            egui::Color32::from_rgb(13, 16, 22)
        })
        .stroke(egui::Stroke::new(0.0_f32, egui::Color32::TRANSPARENT));
    ui.put(rect, button)
}

fn draw_header_status_ring_at(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    color: egui::Color32,
    rect: egui::Rect,
) {
    let response = ui.allocate_rect(rect, egui::Sense::click()).on_hover_text(format!(
        "{}\n{}",
        state.status, state.detail
    ));
    let center = rect.center();
    let radius = 7.5;
    ui.painter().circle_stroke(center, radius, egui::Stroke::new(2.4_f32, color));
    if state.streaming || state.tx_transmitting {
        ui.painter().circle_filled(center, 2.4, color.gamma_multiply(0.85));
    }
    if (state.streaming || state.user_paused) && response.clicked() {
        let pause = state.streaming;
        update_state(shared, |s| {
            s.user_paused = pause;
            s.streaming = false;
            s.status = if pause { "Paused" } else { "Resuming" }.to_string();
            s.detail = if pause {
                "RX paused by user".to_string()
            } else {
                "Restarting FutureSDR UHD source".to_string()
            };
        });
    }
}

fn draw_header_start_stop_at(ui: &mut egui::Ui, shared: &SharedState, state: &SdrUiState, rect: egui::Rect) {
    if state.user_stopped {
        if ui
            .put(rect, egui::Button::new(egui::RichText::new("Start").size(15.0)))
            .on_hover_text("Re-open USB/UHD and start RX")
            .clicked()
        {
            update_state(shared, |s| {
                s.user_stopped = false;
                s.user_paused = false;
                s.streaming = false;
                s.status = "Starting".to_string();
                s.detail = "Scanning USB and opening B210".to_string();
            });
        }
    } else {
        let can_stop = state.opened || state.streaming || state.tx_transmitting || state.user_paused;
        if ui
            .put(
                rect,
                egui::Button::new(egui::RichText::new("Stop").size(15.0)).sense(if can_stop {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                }),
            )
            .on_hover_text("Stop RX, drop UHD handle, and close Android USB connection")
            .clicked()
            && can_stop
        {
            update_state(shared, |s| {
                s.user_stopped = true;
                s.user_paused = false;
                s.streaming = false;
                s.tx_enabled = false;
                s.tx_transmitting = false;
                s.status = "Stopping".to_string();
                s.detail = "Stopping UHD stream and releasing USB device".to_string();
            });
        }
    }
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

fn draw_write_mode(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) {
    draw_tx_settings(ui, shared, state, freq_edit_digit, freq_edit_text);
    ui.add_space(6.0);
    draw_tx_image_spectrum_source(ui, shared, state);
}

fn draw_tx_image_spectrum_source(ui: &mut egui::Ui, shared: &SharedState, state: &SdrUiState) {
    let mut sample_rate_mhz = state.tx_sample_rate / 1e6;
    let mut row_ms = state.tx_row_ms;
    let mut amplitude = state.tx_amplitude;
    let mut loop_enabled = state.tx_loop;
    let mut changed = false;

    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(18, 21, 27))
        .inner_margin(egui::Margin {
            left: 12,
            right: 12,
            top: 12,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                section_label(ui, "IMAGE SPECTRUM");
                let state_text = if state.tx_transmitting {
                    "transmitting"
                } else if state.tx_image_gray.is_empty() {
                    "no image"
                } else {
                    "ready"
                };
                ui.label(
                    egui::RichText::new(state_text)
                        .size(12.0)
                        .color(if state.tx_transmitting {
                            egui::Color32::LIGHT_GREEN
                        } else {
                            egui::Color32::from_rgb(235, 190, 95)
                        }),
                );
            });
            ui.add_space(8.0);
            let image_label = if state.tx_image_name.is_empty() {
                "No image selected".to_string()
            } else {
                format!("{} ({}x{})", state.tx_image_name, state.tx_image_width, state.tx_image_height)
            };
            ui.horizontal(|ui| {
                if ui.button("Choose image…").clicked() {
                    choose_tx_image_from_android(shared);
                }
                let can_transmit = !state.tx_image_gray.is_empty() && state.tx_channels > 0;
                let tx_text = if state.tx_enabled { "Stop TX" } else { "Transmit image" };
                if ui
                    .add_enabled(can_transmit || state.tx_enabled, egui::Button::new(tx_text))
                    .clicked()
                {
                    let starting_tx = !state.tx_enabled;
                    update_state(shared, |s| {
                        s.tx_enabled = starting_tx;
                        s.tx_transmitting = false;
                        s.user_paused = false;
                        s.user_stopped = false;
                        s.tx_revision = s.tx_revision.wrapping_add(1);
                        if starting_tx {
                            // Add the TX branch to the FutureSDR graph. TX remains full-duplex
                            // once active, but the idle graph does not keep a dummy TX streamer.
                            s.graph_revision = s.graph_revision.wrapping_add(1);
                        }
                        s.detail = if s.tx_enabled {
                            "Starting TX image spectrogram".to_string()
                        } else {
                            "Stopping TX image spectrogram".to_string()
                        };
                    });
                }
                let label_width = (ui.available_width() - 4.0).max(48.0);
                let label_text = egui::RichText::new(truncate_middle(&image_label, 34))
                    .size(13.0)
                    .color(egui::Color32::from_rgb(165, 176, 192));
                ui.add_sized(
                    [label_width, 22.0],
                    egui::Label::new(label_text)
                        .truncate()
                        .sense(egui::Sense::hover()),
                )
                .on_hover_text(image_label);
            });
            ui.add_space(8.0);
            changed |= draw_numeric_input_row(
                ui,
                "Sample rate",
                &mut sample_rate_mhz,
                0.2..=10.0,
                0.1,
                3,
                " MS/s",
            );
            changed |= draw_numeric_input_row(ui, "Row time", &mut row_ms, 5.0..=200.0, 1.0, 1, " ms");
            changed |= draw_numeric_input_row(ui, "Amplitude", &mut amplitude, 0.01..=0.95, 0.01, 2, "");
            if ui.checkbox(&mut loop_enabled, "Loop image").changed() {
                changed = true;
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Maps image brightness to frequency-bin power over time; use a shielded/direct-cabled setup.")
                    .size(12.0)
                    .color(egui::Color32::from_rgb(150, 160, 176)),
            );
        });

    if changed {
        sample_rate_mhz = sample_rate_mhz.clamp(0.2, 10.0);
        row_ms = row_ms.clamp(5.0, 200.0);
        amplitude = amplitude.clamp(0.01, 0.95);
        let new_sample_rate = sample_rate_mhz * 1e6;
        let real_change = (new_sample_rate - state.tx_sample_rate).abs() > 1.0
            || (row_ms - state.tx_row_ms).abs() > 0.001
            || (amplitude - state.tx_amplitude).abs() > 0.0005
            || loop_enabled != state.tx_loop;
        if real_change {
            update_state(shared, |s| {
                s.tx_sample_rate = new_sample_rate;
                let lo_limit_hz = new_sample_rate * 0.45;
                s.tx_lo_offset_hz = s.tx_lo_offset_hz.clamp(-lo_limit_hz, lo_limit_hz);
                s.tx_row_ms = row_ms;
                s.tx_amplitude = amplitude;
                s.tx_loop = loop_enabled;
                s.tx_revision = s.tx_revision.wrapping_add(1);
                s.detail = format!(
                    "TX image settings: {:.3} MS/s / {:.1} ms rows",
                    sample_rate_mhz, row_ms
                );
            });
        }
    }
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars || max_chars < 8 {
        return text.to_string();
    }
    let keep = max_chars - 1;
    let left = keep / 2;
    let right = keep - left;
    let prefix: String = text.chars().take(left).collect();
    let suffix: String = text
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn choose_tx_image_from_android(shared: &SharedState) {
    #[cfg(target_os = "android")]
    {
        let vm = ANDROID_VM_PTR.load(Ordering::SeqCst) as *mut std::ffi::c_void;
        let activity = ANDROID_ACTIVITY_PTR.load(Ordering::SeqCst) as *mut std::ffi::c_void;
        if vm.is_null() || activity.is_null() {
            update_state(shared, |s| {
                s.detail = "Android activity handle is not ready".to_string();
            });
            return;
        }
        match file_picker::open_file_picker(vm, activity, TX_IMAGE_PICKER_REQUEST, "image/*") {
            Ok(()) => update_state(shared, |s| {
                s.detail = "Opening Android image picker".to_string();
            }),
            Err(e) => update_state(shared, |s| {
                s.detail = format!("Image picker failed: {e}");
            }),
        }
    }
    #[cfg(not(target_os = "android"))]
    update_state(shared, |s| {
        s.detail = "Image picker is available only on Android".to_string();
    });
}

fn draw_tx_settings(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) {
    let mut center_mhz = state.tx_center_hz / 1e6;
    let mut lo_offset_khz = state.tx_lo_offset_hz / 1e3;
    let mut tx_channel = state.tx_channel;
    let mut tx_gain_db = state.tx_gain_db;
    let mut changed = false;

    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(18, 21, 27))
        .inner_margin(egui::Margin {
            left: 12,
            right: 12,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(9, 12, 17))
                .inner_margin(egui::Margin {
                    left: 12,
                    right: 12,
                    top: 10,
                    bottom: 12,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        changed |= draw_channel_dropdown(ui, "tx_channel_select", &mut tx_channel, state.tx_channels);
                        section_label(ui, "CENTER FREQUENCY");
                    });
                    ui.add_space(6.0);
                    changed |= draw_frequency_digits(ui, &mut center_mhz, freq_edit_digit, freq_edit_text);
                });
            ui.add_space(10.0);
            let lo_limit_khz = (state.tx_sample_rate * 0.45 / 1e3).max(1.0);
            changed |= draw_numeric_input_row(
                ui,
                "LO offset",
                &mut lo_offset_khz,
                -lo_limit_khz..=lo_limit_khz,
                10.0,
                0,
                " kHz",
            );
            changed |= draw_numeric_input_row(ui, "TX gain", &mut tx_gain_db, 0.0..=89.8, 1.0, 1, " dB");
        });

    if changed {
        center_mhz = center_mhz.clamp(50.0, 6000.0);
        tx_gain_db = tx_gain_db.clamp(0.0, 89.8);
        let lo_limit_hz = state.tx_sample_rate * 0.45;
        let new_center_hz = center_mhz * 1e6;
        let new_lo_offset_hz = (lo_offset_khz * 1e3).clamp(-lo_limit_hz, lo_limit_hz);
        let new_tx_channel = tx_channel.min(state.tx_channels.saturating_sub(1));
        let real_change = (new_center_hz - state.tx_center_hz).abs() > 1.0
            || (new_lo_offset_hz - state.tx_lo_offset_hz).abs() > 1.0
            || new_tx_channel != state.tx_channel
            || (tx_gain_db - state.tx_gain_db).abs() > 0.01;
        if real_change {
            update_state(shared, |s| {
                s.tx_center_hz = new_center_hz;
                s.tx_lo_offset_hz = new_lo_offset_hz;
                s.tx_channel = new_tx_channel.min(s.tx_channels.saturating_sub(1));
                s.tx_gain_db = tx_gain_db;
                s.tx_revision = s.tx_revision.wrapping_add(1);
                s.detail = format!(
                    "TX pending: ch{} / {:.6} MHz / LO {:+.0} kHz / {:.1} dB gain",
                    s.tx_channel, center_mhz, s.tx_lo_offset_hz / 1e3, s.tx_gain_db
                );
            });
        }
    }
}

fn draw_controls(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(18, 21, 27))
        .inner_margin(egui::Margin {
            left: 12,
            right: 12,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            draw_tuning_controls(ui, shared, state, freq_edit_digit, freq_edit_text);
        });
}

fn draw_tuning_controls(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) {
    let mut center_mhz = state.center_hz / 1e6;
    let mut bandwidth_mhz = state.bandwidth_hz / 1e6;
    let mut sample_rate_mhz = state.sample_rate / 1e6;
    let mut gain_db = state.gain_db;
    let mut rx_channel = state.rx_channel;
    let mut changed = false;

    egui::Frame::new()
        .fill(egui::Color32::from_rgb(9, 12, 17))
        .inner_margin(egui::Margin {
            left: 12,
            right: 12,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                changed |= draw_channel_dropdown(ui, "rx_channel_select", &mut rx_channel, state.rx_channels);
                section_label(ui, "CENTER FREQUENCY");
            });
            ui.add_space(6.0);
            changed |= draw_frequency_digits(ui, &mut center_mhz, freq_edit_digit, freq_edit_text);

        });

    ui.add_space(10.0);
    changed |= draw_numeric_input_row(
        ui,
        "Bandwidth",
        &mut bandwidth_mhz,
        0.2..=56.0,
        0.1,
        3,
        " MHz",
    );
    changed |= draw_numeric_input_row(
        ui,
        "Sample rate",
        &mut sample_rate_mhz,
        0.2..=61.44,
        0.1,
        3,
        " MS/s",
    );
    changed |= draw_numeric_input_row(ui, "RF gain", &mut gain_db, 0.0..=76.0, 1.0, 1, " dB");

    if changed {
        center_mhz = center_mhz.clamp(50.0, 6000.0);
        sample_rate_mhz = sample_rate_mhz.clamp(0.2, 61.44);
        bandwidth_mhz = bandwidth_mhz.clamp(0.2, sample_rate_mhz.min(56.0));
        gain_db = gain_db.clamp(0.0, 76.0);
        update_state(shared, |s| {
            let new_sample_rate = sample_rate_mhz * 1e6;
            let new_rx_channel = rx_channel.min(s.rx_channels.saturating_sub(1));
            let needs_graph_restart =
                (s.sample_rate - new_sample_rate).abs() > 1.0 || s.rx_channel != new_rx_channel;
            s.center_hz = center_mhz * 1e6;
            s.bandwidth_hz = bandwidth_mhz * 1e6;
            s.sample_rate = new_sample_rate;
            s.gain_db = gain_db;
            s.rx_channel = new_rx_channel;
            s.config_revision = s.config_revision.wrapping_add(1);
            if needs_graph_restart {
                s.graph_revision = s.graph_revision.wrapping_add(1);
            }
            s.detail = format!(
                "Pending tune: ch{} / {:.6} MHz / {:.3} MHz BW / {:.3} MS/s / {:.1} dB",
                s.rx_channel, center_mhz, bandwidth_mhz, sample_rate_mhz, gain_db
            );
        });
    }
}

fn draw_channel_dropdown(
    ui: &mut egui::Ui,
    id: &'static str,
    channel: &mut usize,
    channels: usize,
) -> bool {
    let before = *channel;
    let enabled = channels > 0;
    let selected_text = if enabled {
        format!("CH {}", (*channel).min(channels - 1))
    } else {
        "—".to_string()
    };
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(id)
            .selected_text(selected_text.as_str())
            .width(76.0)
            .show_ui(ui, |ui| {
                for item in 0..channels {
                    ui.selectable_value(channel, item, format!("CH {item}"));
                }
            });
    });
    enabled && *channel != before
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(egui::Color32::from_rgb(135, 145, 160)),
    );
}

fn draw_frequency_digits(
    ui: &mut egui::Ui,
    center_mhz: &mut f64,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) -> bool {
    let hz = (*center_mhz * 1e6).round().clamp(50.0e6, 6000.0e6) as i64;
    let int_mhz = hz / 1_000_000;
    let frac_hz = hz.rem_euclid(1_000_000);
    let int_part = format!("{int_mhz:04}");
    let frac_part = format!("{frac_hz:06}");
    let mut changed = false;

    let available = ui.available_width();
    let cell_w = ((available - 88.0) / 10.0).clamp(18.0, 25.0);
    let digit_font = (cell_w + 3.0).clamp(23.0, 30.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (idx, digit) in int_part.chars().enumerate() {
            changed |= frequency_digit_cell(
                ui,
                digit,
                idx,
                center_mhz,
                cell_w,
                digit_font,
                freq_edit_digit,
                freq_edit_text,
            );
        }
        ui.label(
            egui::RichText::new(".")
                .monospace()
                .size(digit_font)
                .color(egui::Color32::from_rgb(170, 178, 190)),
        );
        for (idx, digit) in frac_part.chars().enumerate() {
            if idx == 3 {
                ui.add_space(3.0);
            }
            changed |= frequency_digit_cell(
                ui,
                digit,
                idx + 4,
                center_mhz,
                cell_w,
                digit_font,
                freq_edit_digit,
                freq_edit_text,
            );
        }
        ui.add_space(3.0);
        ui.label(
            egui::RichText::new("MHz")
                .size(15.0)
                .color(egui::Color32::from_rgb(150, 158, 170)),
        );
    });

    changed
}

fn frequency_digit_cell(
    ui: &mut egui::Ui,
    digit: char,
    digit_index: usize,
    center_mhz: &mut f64,
    cell_w: f32,
    digit_font: f32,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) -> bool {
    if *freq_edit_digit == Some(digit_index) {
        return frequency_digit_editor_cell(
            ui,
            digit,
            digit_index,
            center_mhz,
            cell_w,
            digit_font,
            freq_edit_digit,
            freq_edit_text,
        );
    }

    let size = egui::vec2(cell_w, 52.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let hovered = response.hovered();
    let active = response.dragged();
    let fill = if active {
        egui::Color32::from_rgb(38, 72, 96)
    } else if hovered {
        egui::Color32::from_rgb(31, 39, 50)
    } else {
        egui::Color32::from_rgb(22, 27, 36)
    };
    let stroke = if active || hovered {
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(92, 185, 230))
    } else {
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(45, 52, 64))
    };
    ui.painter().rect(rect, 4.0, fill, stroke, egui::StrokeKind::Inside);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        digit,
        egui::FontId::monospace(digit_font),
        egui::Color32::from_rgb(235, 240, 248),
    );

    if response.clicked() {
        *freq_edit_digit = Some(digit_index);
        *freq_edit_text = digit.to_string();
        return false;
    }

    let mut step_delta = 0;
    let drag_id = response.id.with("digit_drag_residual");
    if response.dragged() {
        let mut residual = ui.data(|data| data.get_temp::<f32>(drag_id).unwrap_or(0.0));
        residual += -response.drag_delta().y;
        let drag_steps = (residual / 7.0).trunc() as i32;
        if drag_steps != 0 {
            step_delta += drag_steps;
            residual -= drag_steps as f32 * 7.0;
        }
        ui.data_mut(|data| data.insert_temp(drag_id, residual));
    } else {
        ui.data_mut(|data| data.insert_temp(drag_id, 0.0));
    }

    step_delta != 0 && tune_frequency_digit_without_carry(center_mhz, digit_index, step_delta)
}

fn frequency_digit_editor_cell(
    ui: &mut egui::Ui,
    digit: char,
    digit_index: usize,
    center_mhz: &mut f64,
    cell_w: f32,
    digit_font: f32,
    freq_edit_digit: &mut Option<usize>,
    freq_edit_text: &mut String,
) -> bool {
    let id = ui.make_persistent_id(("freq_digit_editor", digit_index));
    if freq_edit_text.is_empty() {
        *freq_edit_text = digit.to_string();
    }

    let response = ui
        .scope(|ui| {
            ui.style_mut().visuals.text_cursor.stroke = egui::Stroke::NONE;
            ui.add_sized(
                [cell_w, 52.0],
                egui::TextEdit::singleline(freq_edit_text)
                    .id(id)
                    .font(egui::FontId::monospace(digit_font))
                    .horizontal_align(egui::Align::Center)
                    .desired_width(cell_w)
                    .margin(egui::vec2(0.0, 9.0)),
            )
        })
        .inner;
    response.request_focus();

    let mut changed = false;
    if response.changed() {
        let typed_digit = freq_edit_text.chars().rev().find(|c| c.is_ascii_digit());
        if let Some(typed_digit) = typed_digit {
            *freq_edit_text = typed_digit.to_string();
            changed = set_frequency_digit_without_carry(center_mhz, digit_index, typed_digit);
            if digit_index < 9 {
                *freq_edit_digit = Some(digit_index + 1);
                *freq_edit_text = current_frequency_digit(*center_mhz, digit_index + 1).to_string();
            } else {
                *freq_edit_digit = None;
                freq_edit_text.clear();
            }
        } else {
            freq_edit_text.clear();
        }
    }

    let clicked_outside = ui.input(|input| {
        input.pointer.any_click()
            && input
                .pointer
                .interact_pos()
                .map(|pos| !response.rect.contains(pos))
                .unwrap_or(false)
    });
    if ui.input(|input| input.key_pressed(egui::Key::Enter)) || response.lost_focus() || clicked_outside {
        *freq_edit_digit = None;
        freq_edit_text.clear();
    }

    changed
}

fn current_frequency_digit(center_mhz: f64, digit_index: usize) -> char {
    let hz = (center_mhz * 1e6).round().clamp(50.0e6, 6000.0e6) as i64;
    let int_mhz = hz / 1_000_000;
    let frac_hz = hz.rem_euclid(1_000_000);
    format!("{int_mhz:04}{frac_hz:06}")
        .chars()
        .nth(digit_index)
        .unwrap_or('0')
}

fn tune_frequency_digit_without_carry(center_mhz: &mut f64, digit_index: usize, step_delta: i32) -> bool {
    let Some(old_digit) = current_frequency_digit(*center_mhz, digit_index).to_digit(10) else {
        return false;
    };
    let new_digit = (old_digit as i32 + step_delta).rem_euclid(10) as u32;
    let Some(new_digit) = char::from_digit(new_digit, 10) else {
        return false;
    };
    set_frequency_digit_without_carry(center_mhz, digit_index, new_digit)
}

fn set_frequency_digit_without_carry(center_mhz: &mut f64, digit_index: usize, new_digit: char) -> bool {
    if digit_index >= 10 || !new_digit.is_ascii_digit() {
        return false;
    }
    let hz = (*center_mhz * 1e6).round().clamp(50.0e6, 6000.0e6) as i64;
    let int_mhz = hz / 1_000_000;
    let frac_hz = hz.rem_euclid(1_000_000);
    let mut digits: Vec<char> = format!("{int_mhz:04}{frac_hz:06}").chars().collect();
    if digits[digit_index] == new_digit {
        return false;
    }
    digits[digit_index] = new_digit;
    let int_part: String = digits[..4].iter().collect();
    let frac_part: String = digits[4..].iter().collect();
    let Ok(int_mhz) = int_part.parse::<i64>() else {
        return false;
    };
    let Ok(frac_hz) = frac_part.parse::<i64>() else {
        return false;
    };
    let tuned_hz = int_mhz * 1_000_000 + frac_hz;
    if !(50_000_000..=6_000_000_000).contains(&tuned_hz) {
        return false;
    }
    *center_mhz = tuned_hz as f64 / 1e6;
    true
}

fn draw_numeric_input_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    speed: f64,
    decimals: usize,
    suffix: &str,
) -> bool {
    let mut changed = false;
    let row_height = 25.0;
    let row_left = ui.max_rect().left();
    let row_right = ui.max_rect().right();
    let row_top = ui.cursor().min.y;
    let _ = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::hover());
    let row = egui::Rect::from_min_max(
        egui::pos2(row_left, row_top),
        egui::pos2(row_right, row_top + row_height),
    );
    let y = row.center().y;

    // Grid-like columns: right-aligned label | flexible slider | input | right-aligned unit.
    let label_width = 62.0;
    let input_width = 58.0;
    let unit_width = 40.0;
    let label_slider_gap = 8.0;
    let gap = 8.0;

    let label_right = row.left() + label_width;
    let unit_rect = egui::Rect::from_min_size(
        egui::pos2(row.right() - unit_width, row.top()),
        egui::vec2(unit_width, row_height),
    );
    let input_rect = egui::Rect::from_min_size(
        egui::pos2(unit_rect.left() - gap - input_width, row.top() + 0.5),
        egui::vec2(input_width, 24.0),
    );
    let slider_rect = egui::Rect::from_min_max(
        egui::pos2(label_right + label_slider_gap, row.top() + 1.5),
        egui::pos2((input_rect.left() - gap).max(label_right + 120.0), row.bottom() - 1.5),
    );

    ui.painter().text(
        egui::pos2(label_right, y),
        egui::Align2::RIGHT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(135, 145, 160),
    );
    let slider_response = ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = slider_rect.width();
            ui.put(
                slider_rect,
                egui::Slider::new(value, range.clone())
                    .show_value(false)
                    .step_by(speed),
            )
        })
        .inner;
    changed |= slider_response.changed();
    let drag_response = ui
        .scope(|ui| {
            ui.spacing_mut().button_padding.x = 2.0;
            ui.spacing_mut().interact_size.x = input_rect.width();
            ui.put(
                input_rect,
                egui::DragValue::new(value)
                    .range(range)
                    .speed(speed)
                    .fixed_decimals(decimals),
            )
        })
        .inner;
    changed |= drag_response.changed();
    ui.painter().text(
        egui::pos2(unit_rect.right(), y),
        egui::Align2::RIGHT_CENTER,
        suffix.trim_start(),
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(135, 145, 160),
    );

    ui.add_space(1.0);
    changed
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

fn draw_visual_tabs(ui: &mut egui::Ui, active: &mut VisualView) {
    ui.horizontal(|ui| {
        for view in [VisualView::Spectrum, VisualView::Constellation] {
            if ui.selectable_label(*active == view, view.label()).clicked() {
                *active = view;
            }
        }
    });
}

fn draw_visualization(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    active: VisualView,
    spectrum_pinch_active: &mut bool,
    spectrum_last_commit: &mut Option<Instant>,
) {
    match active {
        VisualView::Spectrum => {
            draw_spectrum(ui, shared, state, spectrum_pinch_active, spectrum_last_commit);
            ui.add_space(6.0);
            draw_waterfall(ui, state);
        }
        VisualView::Constellation => draw_constellation(ui, state),
    }
}

fn draw_spectrum(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    spectrum_pinch_active: &mut bool,
    spectrum_last_commit: &mut Option<Instant>,
) {
    let desired = egui::vec2(ui.available_width(), (ui.available_height() * 0.48).clamp(260.0, 520.0));
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click_and_drag());
    handle_spectrum_gestures(
        ui,
        shared,
        state,
        rect,
        &response,
        spectrum_pinch_active,
        spectrum_last_commit,
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(5, 7, 10));

    draw_grid(&painter, rect);

    if state.frames > 0 {
        let min_db = -120.0;
        let max_db = 0.0;
        let (start, end) = visible_spectrum_bins(state);
        let visible = &state.spectrum_db[start..end];
        let points: Vec<egui::Pos2> = visible
            .iter()
            .enumerate()
            .map(|(i, db)| {
                let x = rect.left() + rect.width() * i as f32 / (visible.len().saturating_sub(1).max(1)) as f32;
                let t = ((*db - min_db) / (max_db - min_db)).clamp(0.0, 1.0);
                let y = rect.bottom() - rect.height() * t;
                egui::pos2(x, y)
            })
            .collect();

        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.6_f32, egui::Color32::from_rgb(90, 220, 120)),
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

fn handle_spectrum_gestures(
    ui: &mut egui::Ui,
    shared: &SharedState,
    state: &SdrUiState,
    rect: egui::Rect,
    response: &egui::Response,
    spectrum_pinch_active: &mut bool,
    spectrum_last_commit: &mut Option<Instant>,
) {
    let span_hz = state.bandwidth_hz.max(1.0);
    let mut handled_pinch = false;
    let mut gesture_changed = false;

    if let Some(touch) = ui.input(|input| input.multi_touch()) {
        if touch.num_touches >= 2 && rect.contains(touch.center_pos) {
            handled_pinch = true;
            *spectrum_pinch_active = true;

            let zoom = f64::from(touch.zoom_delta).clamp(0.25, 4.0);
            let pan_delta_hz = -f64::from(touch.translation_delta.x) / f64::from(rect.width().max(1.0)) * span_hz;
            update_state(shared, |s| {
                if pan_delta_hz.abs() > 1.0 {
                    s.center_hz = (s.center_hz + pan_delta_hz).clamp(50.0e6, 6000.0e6);
                    gesture_changed = true;
                }
                if (zoom - 1.0).abs() > 0.002 {
                    let max_bandwidth = s.sample_rate.min(56.0e6).max(0.2e6);
                    let next = (s.bandwidth_hz / zoom).clamp(0.2e6, max_bandwidth);
                    if (next - s.bandwidth_hz).abs() > 1.0 {
                        s.bandwidth_hz = next;
                        gesture_changed = true;
                    }
                }
            });
            if gesture_changed {
                commit_spectrum_gesture(shared, spectrum_last_commit, false);
            }
        }
    }

    if !handled_pinch && *spectrum_pinch_active {
        *spectrum_pinch_active = false;
        commit_spectrum_gesture(shared, spectrum_last_commit, true);
    }

    if handled_pinch {
        return;
    }

    if response.dragged() {
        let pan_delta_hz = -f64::from(response.drag_delta().x) / f64::from(rect.width().max(1.0)) * span_hz;
        if pan_delta_hz.abs() > 1.0 {
            update_state(shared, |s| {
                s.center_hz = (s.center_hz + pan_delta_hz).clamp(50.0e6, 6000.0e6);
            });
            commit_spectrum_gesture(shared, spectrum_last_commit, false);
        }
    }

    if response.drag_stopped() {
        commit_spectrum_gesture(shared, spectrum_last_commit, true);
    }
}

fn commit_spectrum_gesture(
    shared: &SharedState,
    spectrum_last_commit: &mut Option<Instant>,
    force: bool,
) {
    let now = Instant::now();
    if !force
        && spectrum_last_commit
            .map(|last| now.duration_since(last) < SPECTRUM_GESTURE_DEBOUNCE)
            .unwrap_or(false)
    {
        return;
    }
    *spectrum_last_commit = Some(now);
    update_state(shared, |s| {
        s.bandwidth_hz = s.bandwidth_hz.clamp(0.2e6, s.sample_rate.min(56.0e6).max(0.2e6));
        s.config_revision = s.config_revision.wrapping_add(1);
        s.detail = format!(
            "Pending tune: {:.6} MHz / {:.3} MHz BW / {:.3} MS/s / {:.1} dB",
            s.center_hz / 1e6,
            s.bandwidth_hz / 1e6,
            s.sample_rate / 1e6,
            s.gain_db
        );
    });
}

fn draw_grid(painter: &egui::Painter, rect: egui::Rect) {
    let grid = egui::Color32::from_gray(45);
    for i in 1..10 {
        let x = rect.left() + rect.width() * i as f32 / 10.0;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(0.6_f32, grid),
        );
    }
    for i in 1..6 {
        let y = rect.top() + rect.height() * i as f32 / 6.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.6_f32, grid),
        );
    }
}

fn visible_spectrum_bins(state: &SdrUiState) -> (usize, usize) {
    let n = state.spectrum_db.len().max(1);
    let ratio = (state.bandwidth_hz / state.sample_rate.max(1.0)).clamp(0.0, 1.0);
    let visible = ((n as f64 * ratio).round() as usize).clamp(2, n);
    let start = (n - visible) / 2;
    (start, start + visible)
}

fn draw_constellation(ui: &mut egui::Ui, state: &SdrUiState) {
    let desired = egui::vec2(ui.available_width(), (ui.available_height() - 34.0).clamp(360.0, 760.0));
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(5, 7, 10));

    let plot_side = rect.width().min(rect.height()) * 0.92;
    let plot_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(plot_side, plot_side));
    painter.rect_stroke(
        plot_rect,
        4.0,
        egui::Stroke::new(0.8_f32, egui::Color32::from_gray(50)),
        egui::StrokeKind::Inside,
    );

    let grid = egui::Color32::from_gray(36);
    for i in 1..4 {
        let t = i as f32 / 4.0;
        let x = egui::lerp(plot_rect.left()..=plot_rect.right(), t);
        let y = egui::lerp(plot_rect.top()..=plot_rect.bottom(), t);
        painter.line_segment(
            [egui::pos2(x, plot_rect.top()), egui::pos2(x, plot_rect.bottom())],
            egui::Stroke::new(0.5_f32, grid),
        );
        painter.line_segment(
            [egui::pos2(plot_rect.left(), y), egui::pos2(plot_rect.right(), y)],
            egui::Stroke::new(0.5_f32, grid),
        );
    }
    painter.line_segment(
        [egui::pos2(plot_rect.center().x, plot_rect.top()), egui::pos2(plot_rect.center().x, plot_rect.bottom())],
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(78)),
    );
    painter.line_segment(
        [egui::pos2(plot_rect.left(), plot_rect.center().y), egui::pos2(plot_rect.right(), plot_rect.center().y)],
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(78)),
    );

    if state.constellation.is_empty() {
        painter.text(
            plot_rect.center(),
            egui::Align2::CENTER_CENTER,
            "No IQ samples for constellation",
            egui::FontId::proportional(17.0),
            egui::Color32::GRAY,
        );
        return;
    }

    let mut scale = state
        .constellation
        .iter()
        .map(|c| c.re.abs().max(c.im.abs()))
        .fold(0.0_f32, f32::max);
    scale = (scale * 1.15).clamp(0.05, 2.0);
    let half = plot_rect.width() * 0.5;
    let color = if state.streaming {
        egui::Color32::from_rgba_premultiplied(90, 220, 150, 150)
    } else {
        egui::Color32::from_rgba_premultiplied(78, 135, 96, 130)
    };
    for sample in &state.constellation {
        let x = plot_rect.center().x + (sample.re / scale).clamp(-1.0, 1.0) * half;
        let y = plot_rect.center().y - (sample.im / scale).clamp(-1.0, 1.0) * half;
        painter.circle_filled(egui::pos2(x, y), 1.4, color);
    }

    painter.text(
        plot_rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        format!("{} IQ points", state.constellation.len()),
        egui::FontId::proportional(13.0),
        egui::Color32::GRAY,
    );
}

fn draw_waterfall(ui: &mut egui::Ui, state: &SdrUiState) {
    let desired = egui::vec2(ui.available_width(), (ui.available_height() - 34.0).clamp(180.0, 520.0));
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::BLACK);

    let rows = state.waterfall.len().max(1);
    let (visible_start, visible_end) = visible_spectrum_bins(state);
    let visible_len = visible_end.saturating_sub(visible_start).max(1);
    let bins = ((rect.width() / 3.0).round() as usize)
        .clamp(160, 768)
        .min(visible_len);
    let row_h = rect.height() / WATERFALL_ROWS as f32;
    let bin_w = rect.width() / bins as f32;

    if !state.waterfall.is_empty() {
        for (r, spectrum) in state.waterfall.iter().rev().enumerate() {
            let y0 = rect.bottom() - (r as f32 + 1.0) * row_h;
            if y0 < rect.top() {
                break;
            }
            if spectrum.is_empty() {
                continue;
            }
            let row_start = visible_start.min(spectrum.len().saturating_sub(1));
            let row_end = visible_end.min(spectrum.len()).max(row_start + 1);
            let visible = &spectrum[row_start..row_end];
            for b in 0..bins {
                let idx = b * visible.len() / bins;
                let c = waterfall_color(visible[idx]);
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
