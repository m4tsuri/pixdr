use anyhow::Context;
use eframe::{egui, NativeOptions, Renderer};
#[cfg(target_os = "android")]
use egui_winit::winit;
use log::{error, info};
use num_complex::Complex32;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant};

mod android_uhd_context;
mod futuresdr_backend;
mod usb;
mod uhd_wrapper;

const DEFAULT_FFT_SIZE: usize = 8192;
const WATERFALL_ROWS: usize = 96;
const MAX_CONSTELLATION_POINTS: usize = 2048;
const SAFE_TOP_PAD: i8 = 72;
const SAFE_SIDE_PAD: i8 = 18;
const SAFE_BOTTOM_PAD: i8 = 18;
const SPECTRUM_GESTURE_DEBOUNCE: Duration = Duration::from_millis(250);

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
            Some(usrp) => {
                let usrp = Arc::new(Mutex::new(usrp));
                loop {
                    if let Err(e) = futuresdr_backend::run_b210_flowgraph(usrp.clone(), shared.clone()) {
                        error!("FutureSDR graph stopped: {e:?}");
                        update_state(&shared, |s| {
                            s.status = "B210 opened; RX stream failed".to_string();
                            s.detail = format!("No live samples: {e}");
                            s.streaming = false;
                            // Keep the last spectrum/waterfall frame visible. Retunes and transient
                            // stream setup failures should look frozen, not like a hard reset.
                        });
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            None => thread::sleep(Duration::from_millis(250)),
        }
    });
}

fn apply_rx_config(usrp: &mut uhd::Usrp, shared: &SharedState) -> anyhow::Result<()> {
    let (center_hz, mut bandwidth_hz, sample_rate, gain_db, revision) = {
        let state = shared.lock().unwrap();
        (
            state.center_hz,
            state.bandwidth_hz,
            state.sample_rate,
            state.gain_db,
            state.config_revision,
        )
    };
    bandwidth_hz = bandwidth_hz.min(sample_rate);

    update_state(shared, |s| {
        s.status = "Configuring RX".to_string();
        s.detail = format!(
            "center={:.6} MHz, bandwidth={:.3} MHz, rate={:.3} MS/s, gain={:.1} dB",
            center_hz / 1e6,
            bandwidth_hz / 1e6,
            sample_rate / 1e6,
            gain_db
        );
    });

    info!(
        "Applying RX config rev={revision}: center={:.6} MHz bandwidth={:.3} MHz sample_rate={:.3} MS/s gain={:.1} dB",
        center_hz / 1e6,
        bandwidth_hz / 1e6,
        sample_rate / 1e6,
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
            "RX tuned: {:.6} MHz / {:.3} MHz BW / {:.3} MS/s",
            center_hz / 1e6,
            bandwidth_hz / 1e6,
            sample_rate / 1e6
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
                draw_header(ui, &state);
                ui.add_space(4.0);
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
            section_label(ui, "CENTER FREQUENCY");
            ui.add_space(6.0);
            changed |= draw_frequency_digits(ui, &mut center_mhz, freq_edit_digit, freq_edit_text);
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Slide a digit vertically to change only that digit. Tap a digit to edit it in place.")
                    .size(11.0)
                    .color(egui::Color32::from_rgb(115, 125, 140)),
            );
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
            s.center_hz = center_mhz * 1e6;
            s.bandwidth_hz = bandwidth_mhz * 1e6;
            s.sample_rate = sample_rate_mhz * 1e6;
            s.gain_db = gain_db;
            s.config_revision = s.config_revision.wrapping_add(1);
            s.detail = format!(
                "Pending tune: {:.6} MHz / {:.3} MHz BW / {:.3} MS/s / {:.1} dB",
                center_mhz, bandwidth_mhz, sample_rate_mhz, gain_db
            );
        });
    }
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
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(9, 12, 17))
        .inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 5,
            bottom: 5,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                ui.add_sized(
                    [112.0, 24.0],
                    egui::Label::new(
                        egui::RichText::new(label)
                            .size(13.0)
                            .color(egui::Color32::from_rgb(135, 145, 160)),
                    ),
                );
                changed |= ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::DragValue::new(value)
                            .range(range)
                            .speed(speed)
                            .fixed_decimals(decimals)
                            .suffix(suffix),
                    )
                    .changed();
            });
        });
    ui.add_space(4.0);
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
        format!("{:.6} MHz", (state.center_hz - state.sample_rate / 2.0) / 1e6),
        egui::FontId::proportional(13.0),
        egui::Color32::GRAY,
    );
    painter.text(
        rect.right_bottom() + egui::vec2(-8.0, -8.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{:.6} MHz", (state.center_hz + state.sample_rate / 2.0) / 1e6),
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
    let span_hz = state.sample_rate.max(1.0);
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
    let bins = ((rect.width() / 3.0).round() as usize)
        .clamp(160, 768)
        .min(state.spectrum_db.len().max(1));
    let row_h = rect.height() / WATERFALL_ROWS as f32;
    let bin_w = rect.width() / bins as f32;

    if !state.waterfall.is_empty() {
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
