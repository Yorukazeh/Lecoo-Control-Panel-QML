use std::{
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, ChildStderr, Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use cxx_qt::Threading;
use cxx_qt::casting::Upcast;
use cxx_qt_lib::{QString, QTime};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Highest fan duty accepted by the supported profiles.
const FAN_PWM_MAX: i32 = 220;
const LED_PRESETS: &[&str] = &[
    "smooth",
    "sleep",
    "alert",
    "zen",
    "ping",
    "energetic",
    "warning",
    "vacuum",
    "panic",
    "sonar",
    "toxic",
];

/// A long-running CLI monitor process and the handle needed to stop it.
struct MonitorProcess {
    child: Mutex<Option<Child>>,
}

impl MonitorProcess {
    fn new(child: Child) -> Self {
        Self { child: Mutex::new(Some(child)) }
    }

    fn stop(&self) {
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(mut child) = child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Default)]
struct MonitorState {
    generation: u64,
    process: Option<Arc<MonitorProcess>>,
}

impl MonitorState {
    fn stop(&mut self) {
        if let Some(process) = self.process.take() {
            process.stop();
        }
    }
}

/// Six temperature breakpoints from 40 °C to 90 °C.
const FAN_CURVE_TEMPS: [i32; 6] = [40, 50, 60, 70, 80, 90];

struct FanCurve {
    enabled: bool,
    points: [i32; 6],
    last_pwm: i32,
    last_applied: Option<Instant>,
}

impl Default for FanCurve {
    fn default() -> Self {
        Self {
            enabled: false,
            points: [60, 80, 100, 130, 170, 220],
            last_pwm: -1,
            last_applied: None,
        }
    }
}

impl FanCurve {
    fn pwm_for(&self, temperature: i32) -> i32 {
        let temperature = temperature.clamp(FAN_CURVE_TEMPS[0], FAN_CURVE_TEMPS[5]);
        if temperature == FAN_CURVE_TEMPS[5] {
            return self.points[5];
        }

        let offset = temperature - FAN_CURVE_TEMPS[0];
        let index = (offset / 10) as usize;
        let fraction = (offset % 10) as f32 / 10.0;
        let lower = self.points[index] as f32;
        let upper = self.points[index + 1] as f32;
        (lower + (upper - lower) * fraction).round() as i32
    }

    fn reset(&mut self) {
        self.last_pwm = -1;
    }
}

enum ChargeAction {
    Preset(String),
    Range(u8, u8),
}

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
        include!("cxx-qt-lib/qstringlist.h");
        type QStringList = cxx_qt_lib::QStringList;
        include!("cxx-qt-lib/qguiapplication.h");
        type QGuiApplication = cxx_qt_lib::QGuiApplication;
    }

    // Expose Qt application identity helpers to QML.
    unsafe extern "C++" {
        include!("qt_shim.h");
        #[rust_name = "set_application_icon"]
        fn lecooSetApplicationIcon();
        #[rust_name = "set_desktop_file_name"]
        fn lecooSetDesktopFileName(name: &QString);
        #[rust_name = "set_quick_controls_style"]
        fn lecooSetQuickControlsStyle(style: &QString);
        #[rust_name = "shim_system_font_family"]
        fn lecooSystemFontFamily() -> QString;
        #[rust_name = "shim_fixed_font_family"]
        fn lecooFixedFontFamily() -> QString;
        #[rust_name = "cursor_screen_name"]
        fn lecooCursorScreenName() -> QString;
        #[rust_name = "platform_name"]
        fn lecooPlatformName() -> QString;
        #[rust_name = "os_product_name"]
        fn lecooOsName() -> QString;
        #[rust_name = "os_kernel_version"]
        fn lecooOsVersion() -> QString;
        #[rust_name = "new_application"]
        fn lecooNewApplication(args: &QStringList) -> UniquePtr<QGuiApplication>;
        #[rust_name = "start_single_instance"]
        fn lecooStartSingleInstance() -> i32;
        #[rust_name = "tray_available"]
        fn lecooTrayAvailable() -> bool;
        #[rust_name = "tray_init"]
        fn lecooInitTray(controller: Pin<&mut QObject>);
        #[rust_name = "tray_set_menu"]
        fn lecooTraySetMenu(items: &QStringList, checked_index: i32, enabled: bool);
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(bool, connected, cxx_name = "connected")]
        #[qproperty(bool, busy, cxx_name = "busy")]
        #[qproperty(bool, monitoring, cxx_name = "monitoring")]
        #[qproperty(i32, cpu_temperature, cxx_name = "cpuTemperature")]
        #[qproperty(i32, system_temperature, cxx_name = "systemTemperature")]
        #[qproperty(i32, cpu_fan_rpm, cxx_name = "cpuFanRpm")]
        #[qproperty(i32, gpu_fan_rpm, cxx_name = "gpuFanRpm")]
        #[qproperty(QString, power_profile, cxx_name = "powerProfile")]
        #[qproperty(QString, controller_info, cxx_name = "controllerInfo")]
        #[qproperty(QString, battery_info, cxx_name = "batteryInfo")]
        #[qproperty(QString, charge_status, cxx_name = "chargeStatus")]
        #[qproperty(QStringList, charge_presets, cxx_name = "chargePresets")]
        #[qproperty(bool, custom_charge_supported, cxx_name = "customChargeSupported")]
        #[qproperty(bool, charge_busy, cxx_name = "chargeBusy")]
        #[qproperty(QString, os_info, cxx_name = "osInfo")]
        #[qproperty(QString, os_version, cxx_name = "osVersion")]
        #[qproperty(QString, status_message, cxx_name = "statusMessage")]
        #[qproperty(QString, error_message, cxx_name = "errorMessage")]
        #[qproperty(QString, last_update, cxx_name = "lastUpdate")]
        #[qproperty(QString, app_version, cxx_name = "appVersion")]
        #[qproperty(QString, daemon_board, cxx_name = "daemonBoard")]
        #[qproperty(QString, daemon_version, cxx_name = "daemonVersion")]
        type ControlCenter = super::ControlCenterRust;

        #[qinvokable]
        #[cxx_name = "refreshAll"]
        fn refresh_all(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "refreshInfo"]
        fn refresh_info(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "refreshPowerProfile"]
        fn refresh_power_profile(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "toggleMonitoring"]
        fn toggle_monitoring(self: Pin<&mut Self>, enabled: bool);

        #[qinvokable]
        #[cxx_name = "applyPowerProfile"]
        fn apply_power_profile(self: Pin<&mut Self>, profile: &QString);

        #[qinvokable]
        #[cxx_name = "applyFanMode"]
        fn apply_fan_mode(self: Pin<&mut Self>, target: &QString, mode: &QString, pwm: i32);

        #[qinvokable]
        #[cxx_name = "setFanCurve"]
        fn set_fan_curve(self: Pin<&mut Self>, target: &QString, enabled: bool, points: &QString);

        #[qinvokable]
        #[cxx_name = "setFanCurveInterval"]
        fn set_fan_curve_interval(self: Pin<&mut Self>, millis: i32);

        #[qinvokable]
        #[cxx_name = "applyChargePreset"]
        fn apply_charge_preset(self: Pin<&mut Self>, preset: &QString);

        #[qinvokable]
        #[cxx_name = "applyChargeRange"]
        fn apply_charge_range(self: Pin<&mut Self>, min: i32, max: i32);

        #[qinvokable]
        #[cxx_name = "refreshCharge"]
        fn refresh_charge(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "applyKeyboardLevel"]
        fn apply_keyboard_level(self: Pin<&mut Self>, level: i32);

        #[qinvokable]
        #[cxx_name = "applyKeyboardMode"]
        fn apply_keyboard_mode(self: Pin<&mut Self>, mode: &QString, pwm: i32);

        #[qinvokable]
        #[cxx_name = "applyLedMode"]
        fn apply_led_mode(self: Pin<&mut Self>, mode: &QString, value: i32, preset: &QString, sleep: bool);

        #[qinvokable]
        #[cxx_name = "ledPresets"]
        fn led_presets(&self) -> QStringList;

        #[qinvokable]
        #[cxx_name = "refreshDaemonInfo"]
        fn refresh_daemon_info(self: Pin<&mut Self>);

        // Platform default font families, read once at startup (no text font is bundled).
        #[qinvokable]
        #[cxx_name = "systemFontFamily"]
        fn system_font_family(&self) -> QString;

        #[qinvokable]
        #[cxx_name = "fixedFontFamily"]
        fn fixed_font_family(&self) -> QString;

        // Return the screen under the cursor.
        #[qinvokable]
        #[cxx_name = "cursorScreenName"]
        fn cursor_screen_name(&self) -> QString;

        #[qinvokable]
        #[cxx_name = "platformName"]
        fn platform_name(&self) -> QString;

        // Return a compiled-in legal document by name.
        #[qinvokable]
        #[cxx_name = "legalDocument"]
        fn legal_document(&self, name: &QString) -> QString;

        #[qinvokable]
        #[cxx_name = "trayAvailable"]
        fn tray_available(&self) -> bool;

        #[qinvokable]
        #[cxx_name = "trayInit"]
        fn init_tray(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "traySetMenu"]
        fn tray_set_menu(self: Pin<&mut Self>, items: &QStringList, profile_index: i32, enabled: bool);
        #[qsignal]
        #[cxx_name = "trayAction"]
        fn tray_action(self: Pin<&mut Self>, action: i32);
        #[qinvokable]
        #[cxx_name = "handleTrayAction"]
        fn handle_tray_action(self: Pin<&mut Self>, action: i32);
    }

    // Enables `self.qt_thread()` so CLI work can run off the Qt event loop.
    impl cxx_qt::Threading for ControlCenter {}
}

pub struct ControlCenterRust {
    connected: bool,
    busy: bool,
    monitoring: bool,
    cpu_temperature: i32,
    system_temperature: i32,
    cpu_fan_rpm: i32,
    gpu_fan_rpm: i32,
    power_profile: QString,
    controller_info: QString,
    battery_info: QString,
    charge_status: QString,
    charge_presets: cxx_qt_lib::QStringList,
    custom_charge_supported: bool,
    charge_busy: bool,
    os_info: QString,
    os_version: QString,
    status_message: QString,
    error_message: QString,
    last_update: QString,
    app_version: QString,
    daemon_board: QString,
    daemon_version: QString,
    monitor: Arc<Mutex<MonitorState>>,
    fan_curves: Arc<Mutex<[FanCurve; 2]>>,
    fan_curve_interval: Arc<Mutex<u64>>,
}

impl Default for ControlCenterRust {
    fn default() -> Self {
        Self {
            connected: false,
            busy: false,
            monitoring: false,
            cpu_temperature: 0,
            system_temperature: 0,
            cpu_fan_rpm: 0,
            gpu_fan_rpm: 0,
            power_profile: QString::default(),
            controller_info: QString::default(),
            battery_info: QString::default(),
            charge_status: QString::default(),
            charge_presets: cxx_qt_lib::QStringList::default(),
            custom_charge_supported: false,
            charge_busy: false,
            os_info: qobject::os_product_name(),
            os_version: qobject::os_kernel_version(),
            status_message: QString::default(),
            error_message: QString::default(),
            last_update: QString::default(),
            app_version: QString::from(env!("CARGO_PKG_VERSION")),
            daemon_board: QString::default(),
            daemon_version: QString::default(),
            monitor: Arc::new(Mutex::new(MonitorState::default())),
            fan_curves: Arc::new(Mutex::new([FanCurve::default(), FanCurve::default()])),
            fan_curve_interval: Arc::new(Mutex::new(2000)),
        }
    }
}

impl Drop for ControlCenterRust {
    fn drop(&mut self) {
        let mut state = self.monitor.lock().unwrap_or_else(|error| error.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.stop();
        drop(state);
        // Do not leave the firmware in the last curve PWM after closing.
        if let Some(requests) = fan_requests("both", "auto", 0) {
            let _ = daemon_write_or_cli(&requests, &["fan", "both", "auto"]);
        }
    }
}

/// Resolve the CLI override or the executable found on PATH.
fn cli_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(path) = std::env::var("LECOO_CTRL") {
        let path = path.trim();
        if !path.is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }

    candidates.push(PathBuf::from(if cfg!(windows) { "lecoo-ctrl.exe" } else { "lecoo-ctrl" }));
    candidates
}

fn run_command(binary: &Path, args: &[&str]) -> std::io::Result<Output> {
    let mut command = Command::new(binary);
    command.args(args);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command.output()
}

fn command_result(output: Output) -> Result<String, String> {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

    if output.status.success() {
        Ok(stdout)
    } else if stderr.is_empty() {
        Err(stdout)
    } else {
        Err(stderr)
    }
}

fn run_cli(args: &[&str]) -> Result<String, String> {
    for candidate in cli_candidates() {
        match run_command(&candidate, args) {
            Ok(output) => return command_result(output),
            Err(_) => continue,
        }
    }
    Err("backend.error.cli_unavailable".to_owned())
}

/// Start a CLI process with piped output for long-running monitoring.
fn spawn_cli(args: &[&str]) -> io::Result<Child> {
    let mut last_error = None;
    for candidate in cli_candidates() {
        let mut command = Command::new(&candidate);
        command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "lecoo-ctrl was not found")))
}

/// Read one CR- or LF-terminated monitor update.
fn read_monitor_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<usize> {
    line.clear();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(line.len());
        }

        if let Some(index) = buffer.iter().position(|byte| matches!(byte, b'\r' | b'\n')) {
            let skip = usize::from(buffer[index] == b'\r' && buffer.get(index + 1) == Some(&b'\n'));
            let consumed = index + 1 + skip;
            if index == 0 && line.is_empty() {
                reader.consume(consumed);
                continue;
            }
            line.extend_from_slice(&buffer[..index]);
            reader.consume(consumed);
            return Ok(line.len());
        }

        line.extend_from_slice(buffer);
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

/// Collect the stderr text of a monitor process, which exits with it.
fn read_stderr(mut stderr: ChildStderr) -> String {
    let mut text = String::new();
    let _ = stderr.read_to_string(&mut text);
    text.trim().to_owned()
}

fn numbers(text: &str) -> Vec<i32> {
    text.split(|ch: char| !ch.is_ascii_digit())
        .filter_map(|part| part.parse::<i32>().ok())
        .collect()
}

fn parse_curve_points(text: &str) -> Option<[i32; 6]> {
    let values: Vec<i32> = text.split(',').filter_map(|part| part.trim().parse::<i32>().ok()).collect();
    if values.len() != 6 {
        return None;
    }
    let mut points = [0; 6];
    for (index, value) in values.into_iter().enumerate() {
        points[index] = value.clamp(0, FAN_PWM_MAX);
    }
    for index in 1..points.len() {
        points[index] = points[index].max(points[index - 1]);
    }
    Some(points)
}

/// Parse one monitoring update: CPU temp, system temp, CPU fan, GPU fan.
fn monitoring_values(line: &str) -> Option<(i32, i32, i32, i32)> {
    let values = numbers(line);
    if values.len() >= 4 { Some((values[0], values[1], values[2], values[3])) } else { None }
}

/// Return the local time as HH:MM:SS.
fn now_label() -> String {
    let now = QTime::current_time();
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

fn led_args(mode: &str, value: i32, preset: &str, sleep: bool) -> Option<Vec<String>> {
    let mut args = vec!["led".to_owned()];
    if sleep {
        args.push("sleep".to_owned());
    }
    args.push(mode.to_owned());
    match mode {
        "auto" => {}
        "custom" => args.push(value.clamp(0, 255).to_string()),
        "breath" if LED_PRESETS.contains(&preset) => args.push(preset.to_owned()),
        "flash" if !sleep && LED_PRESETS.contains(&preset) && (1..=65_535).contains(&value) => {
            args.extend([preset.to_owned(), "--for".to_owned(), format!("{:.3}", f64::from(value) / 1000.0)]);
        }
        "overheat" if !sleep && (0..=255).contains(&value) => {
            let threshold = if value == 0 { "off".to_owned() } else { value.to_string() };
            return Some(vec!["daemon".to_owned(), "overheat".to_owned(), threshold]);
        }
        _ => return None,
    }
    Some(args)
}

fn qstring(value: impl Into<String>) -> QString {
    QString::from(value.into())
}

fn read_daemon<T>(
    native: impl FnOnce(&mut ipc::IpcClient) -> Result<T, String>,
    legacy: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    // Keep explicit CLI overrides and older daemon versions working during migration.
    if std::env::var("LECOO_CTRL").is_ok_and(|path| !path.trim().is_empty()) {
        return legacy();
    }
    match ipc::IpcClient::connect() {
        Ok(mut client) => native(&mut client).or_else(|_| legacy()),
        Err(_) => legacy(),
    }
}

fn daemon_response(
    client: &mut ipc::IpcClient,
    request: ipc::IpcRequest,
) -> Result<ipc::IpcResponse, String> {
    match client.request(&request).map_err(|error| error.to_string())? {
        ipc::IpcResponse::Error(error) => Err(error.message),
        response => Ok(response),
    }
}

fn daemon_temperatures(response: ipc::IpcResponse) -> Result<(i32, i32), String> {
    match response {
        ipc::IpcResponse::Temps { cpu_c, sys_c } => Ok((cpu_c.into(), sys_c.into())),
        _ => Err("backend.error.parse_temps".to_owned()),
    }
}

fn daemon_fans(response: ipc::IpcResponse) -> Result<(i32, i32), String> {
    match response {
        ipc::IpcResponse::FanRpm { cpu, gpu } => Ok((cpu.into(), gpu.into())),
        _ => Err("backend.error.parse_fans".to_owned()),
    }
}

fn power_request(profile: &str) -> Option<ipc::IpcRequest> {
    use ipc::IpcRequest;
    use lecoo_types::ec_types::PowerProfile;

    let profile = match profile {
        "silent" => PowerProfile::Silent,
        "default" => PowerProfile::Default,
        "perf" => PowerProfile::Performance,
        _ => return None,
    };
    Some(IpcRequest::SetPowerProfile(profile))
}

fn fan_requests(target: &str, mode: &str, pwm: i32) -> Option<Vec<ipc::IpcRequest>> {
    use ipc::IpcRequest;
    use lecoo_types::ec_types::{FanIndex, FanMode};

    let mode = match mode {
        "auto" => FanMode::Auto,
        "full" => FanMode::Full,
        "turbo" => FanMode::Turbo,
        "custom" => FanMode::Custom(pwm.clamp(0, FAN_PWM_MAX) as u8),
        _ => return None,
    };
    let fans: &[FanIndex] = match target {
        "cpu" => &[FanIndex::Cpu],
        "gpu" => &[FanIndex::Gpu],
        "both" => &[FanIndex::Cpu, FanIndex::Gpu],
        _ => return None,
    };
    Some(fans.iter().map(|fan| IpcRequest::SetFanMode { fan: *fan, mode }).collect())
}

fn keyboard_request(mode: &str, pwm: i32) -> Option<ipc::IpcRequest> {
    use ipc::IpcRequest;
    use lecoo_types::ec_types::KeyboardBacklightLevel;

    let level = match mode {
        "off" => KeyboardBacklightLevel::Off,
        "low" => KeyboardBacklightLevel::Low,
        "medium" => KeyboardBacklightLevel::Medium,
        "high" => KeyboardBacklightLevel::High,
        "custom" => KeyboardBacklightLevel::Custom(pwm.clamp(0, 255) as u8),
        _ => return None,
    };
    Some(IpcRequest::SetKeyboardBacklight(level))
}

fn led_request(mode: &str, value: i32, preset: &str, sleep: bool) -> Option<ipc::IpcRequest> {
    use ipc::{DaemonCommand, IpcRequest};
    use lecoo_types::ec_types::{BreathConfig, PowerLedMode};

    if mode == "overheat" && !sleep && (0..=255).contains(&value) {
        return Some(IpcRequest::DaemonCommand(DaemonCommand::SetOverheatAlert(
            (value != 0).then_some(value as u8),
        )));
    }
    if mode == "flash" && !sleep && (1..=u16::MAX as i32).contains(&value) {
        return BreathConfig::by_name(preset)
            .map(|effect| IpcRequest::FlashLed { effect, duration_ms: value as u16 });
    }
    let led_mode = match mode {
        "auto" => PowerLedMode::Auto,
        "custom" => PowerLedMode::Custom(value.clamp(0, 255) as u8),
        "breath" => PowerLedMode::Animation(BreathConfig::by_name(preset)?),
        _ => return None,
    };
    Some(if sleep {
        IpcRequest::DaemonCommand(DaemonCommand::SetLedOnSleep(led_mode))
    } else {
        IpcRequest::SetLedMode(led_mode)
    })
}

fn daemon_write(requests: &[ipc::IpcRequest]) -> Result<Option<()>, String> {
    if std::env::var("LECOO_CTRL").is_ok_and(|path| !path.trim().is_empty()) {
        return Ok(None);
    }
    let Ok(mut client) = ipc::IpcClient::connect() else {
        return Ok(None);
    };
    for request in requests {
        match client.request(request).map_err(|error| error.to_string())? {
            ipc::IpcResponse::Success => {}
            ipc::IpcResponse::Error(error) if error.code == ipc::ErrorCode::UnsupportedRequest => {
                return Ok(None);
            }
            ipc::IpcResponse::Error(error) => return Err(error.message),
            _ => return Err("backend.error.unexpected_daemon_response".to_owned()),
        }
    }
    Ok(Some(()))
}

fn daemon_write_with_client(client: &mut ipc::IpcClient, request: &ipc::IpcRequest) -> Result<bool, String> {
    match client.request(request).map_err(|error| error.to_string())? {
        ipc::IpcResponse::Success => Ok(true),
        ipc::IpcResponse::Error(error) if error.code == ipc::ErrorCode::UnsupportedRequest => Ok(false),
        ipc::IpcResponse::Error(error) => Err(error.message),
        _ => Err("backend.error.unexpected_daemon_response".to_owned()),
    }
}

fn daemon_write_or_cli(requests: &[ipc::IpcRequest], args: &[&str]) -> Result<(), String> {
    match daemon_write(requests)? {
        Some(()) => Ok(()),
        None => run_cli(args).map(|_| ()),
    }
}

fn preset_order(name: &str) -> u8 {
    match name {
        "full" => 0,
        "high" => 1,
        "balanced" => 2,
        "lifespan" => 3,
        "desk" => 4,
        "hold" => 5,
        _ => u8::MAX,
    }
}

fn charge_presets_from_caps(caps: &lecoo_types::caps::Capabilities) -> cxx_qt_lib::QStringList {
    let mut names: Vec<_> = caps.charge.presets.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_by_key(|name| preset_order(name));
    let mut presets = cxx_qt_lib::QStringList::default();
    for name in names {
        presets.append(qstring(name));
    }
    presets
}

fn charge_intent_name(intent: &lecoo_types::ec_types::ChargeIntent) -> String {
    use lecoo_types::ec_types::ChargeIntent;
    match intent {
        ChargeIntent::Full => "full".to_owned(),
        ChargeIntent::Preserve(Some(range)) => format!("range:{}:{}", range.min, range.max),
        ChargeIntent::Preserve(None) => "preserve".to_owned(),
        ChargeIntent::Freeze => "hold".to_owned(),
    }
}

fn charge_status_summary(status: &lecoo_types::caps::ChargeStatus) -> String {
    format!(
        "ipc|{}|{}|{}|{}",
        charge_intent_name(&status.desired),
        charge_intent_name(&status.effective),
        status.soc,
        status.pending.as_deref().unwrap_or("")
    )
}

fn battery_info_ipc(
    info: &lecoo_types::ec_types::BatteryInfo,
    status: Option<&lecoo_types::caps::ChargeStatus>,
) -> String {
    let state = match info.current_ma.signum() {
        1 => "charging",
        -1 => "discharging",
        _ => "idle",
    };
    let power = if info.power_w() != 0.0 { format!("{:+.1} W", info.power_w()) } else { String::new() };
    let voltage = format!("{:.2}", info.voltage_mv as f32 / 1000.0);
    let voltage_cell = info.cell_mv().map(|cell| format!("{:.2}", cell as f32 / 1000.0)).unwrap_or_default();
    let percent = info.charge_pct().map(|value| format!("{value:.1}")).unwrap_or_default();
    let health = info.health_pct().map(|value| format!("{value:.1}")).unwrap_or_default();
    let limit = status.map(|status| charge_intent_name(&status.effective)).unwrap_or_default();
    let eta = status
        .and_then(|status| info.eta_min(charge_stop_percent(status)))
        .map(|minutes| minutes.to_string())
        .unwrap_or_default();
    format!(
        "ipc|{state}|{power}|{voltage}|{voltage_cell}|{}|{}|{percent}|{:.1}|{:.1}|{health}|{}|{limit}|{eta}",
        info.remaining_mah,
        info.full_mah,
        info.remaining_wh(),
        info.full_wh(),
        info.cycles,
    )
}

fn charge_stop_percent(status: &lecoo_types::caps::ChargeStatus) -> u8 {
    use lecoo_types::ec_types::ChargeIntent;
    match status.effective {
        ChargeIntent::Preserve(Some(range)) => range.max,
        ChargeIntent::Preserve(None) => status.thresholds.map_or(100, |(_, max)| max),
        ChargeIntent::Full | ChargeIntent::Freeze => 100,
    }
}

fn charge_intent_from_preset(
    caps: &lecoo_types::caps::Capabilities,
    preset: &str,
) -> Option<lecoo_types::ec_types::ChargeIntent> {
    caps.charge
        .presets
        .iter()
        .find_map(|(name, intent)| name.eq_ignore_ascii_case(preset).then_some(*intent))
}

fn native_charge_presets_response(
    response: ipc::IpcResponse,
) -> Result<lecoo_types::caps::Capabilities, String> {
    match response {
        ipc::IpcResponse::Capabilities(caps) => Ok(*caps),
        _ => Err("backend.error.unexpected_capabilities".to_owned()),
    }
}

fn native_charge_status_response(response: ipc::IpcResponse) -> Result<String, String> {
    match response {
        ipc::IpcResponse::ChargeStatus(status) => Ok(charge_status_summary(&status)),
        _ => Err("backend.error.unexpected_charge_status".to_owned()),
    }
}

fn native_battery_response(response: ipc::IpcResponse) -> Result<lecoo_types::ec_types::BatteryInfo, String> {
    match response {
        ipc::IpcResponse::BatteryInfo(info) => Ok(info),
        _ => Err("backend.error.unexpected_battery_info".to_owned()),
    }
}

fn charge_action_request(
    caps: &lecoo_types::caps::Capabilities,
    action: &ChargeAction,
) -> Result<ipc::IpcRequest, String> {
    use lecoo_types::ec_types::{ChargeIntent, ChargeRange};

    let intent = match action {
        ChargeAction::Preset(name) => charge_intent_from_preset(caps, name)
            .ok_or_else(|| "backend.error.unsupported_charge_mode".to_owned())?,
        ChargeAction::Range(min, max) => {
            let Some((low, high)) = caps.charge.custom_range else {
                return Err("backend.error.unsupported_charge_range".to_owned());
            };
            if min >= max || *min < low || *max > high {
                return Err("charge.invalidRange".to_owned());
            }
            ChargeIntent::Preserve(Some(ChargeRange { min: *min, max: *max }))
        }
    };
    Ok(ipc::IpcRequest::SetChargeIntent(intent))
}

fn apply_charge_native_or_cli(action: ChargeAction, cli_args: Vec<String>) -> Result<String, String> {
    let legacy = || {
        let args: Vec<_> = std::iter::once("charge").chain(cli_args.iter().map(String::as_str)).collect();
        run_cli(&args).map(|_| run_cli(&["charge"]).unwrap_or_default())
    };
    if std::env::var("LECOO_CTRL").is_ok_and(|path| !path.trim().is_empty()) {
        return legacy();
    }
    let Ok(mut client) = ipc::IpcClient::connect() else {
        return legacy();
    };
    let caps = match daemon_response(
        &mut client,
        ipc::IpcRequest::DaemonCommand(ipc::DaemonCommand::GetCapabilities),
    )
    .and_then(native_charge_presets_response)
    {
        Ok(caps) => caps,
        Err(_) => return legacy(),
    };
    let request = match charge_action_request(&caps, &action) {
        Ok(request) => request,
        Err(error)
            if matches!(
                error.as_str(),
                "backend.error.unsupported_charge_mode" | "backend.error.unsupported_charge_range"
            ) =>
        {
            return legacy();
        }
        Err(error) => return Err(error),
    };
    if !daemon_write_with_client(&mut client, &request)? {
        return legacy();
    }
    daemon_response(&mut client, ipc::IpcRequest::GetChargeStatus).and_then(native_charge_status_response)
}

fn power_profile_from_response(response: ipc::IpcResponse) -> Result<String, String> {
    match response {
        ipc::IpcResponse::PowerLimit(profile) => Ok(match profile {
            lecoo_types::ec_types::PowerProfile::Silent => "silent",
            lecoo_types::ec_types::PowerProfile::Default => "default",
            lecoo_types::ec_types::PowerProfile::Performance => "perf",
        }
        .to_owned()),
        _ => Err("backend.error.unexpected_power_response".to_owned()),
    }
}

fn read_sensor_pair(command: &str, error: &str) -> Result<(i32, i32), String> {
    match numbers(&run_cli(&[command])?).as_slice() {
        [first, second, ..] => Ok((*first, *second)),
        _ => Err(error.to_owned()),
    }
}

fn battery_details(output: &str) -> &str {
    // The card already has a title; all upstream locales prefix the CLI heading with 🔋.
    let (first, rest) = output.split_once('\n').unwrap_or((output, ""));
    if first.trim_start().starts_with('🔋') { rest.trim_end() } else { output }
}

fn charge_range_args(min: i32, max: i32) -> Option<[String; 4]> {
    (20 <= min && min < max && max <= 100)
        .then(|| ["--min".into(), min.to_string(), "--max".into(), max.to_string()])
}

fn supports_charge_range(help: &str) -> bool {
    help.lines().any(|line| line.trim_start().starts_with("--min <N> --max <N>"))
}

fn charge_presets(help: &str) -> cxx_qt_lib::QStringList {
    const SUPPORTED: &[&str] = &["full", "desk", "lifespan", "balanced", "high", "hold"];
    let mut presets = Vec::new();
    let mut in_presets = false;
    for line in help.lines() {
        let line = line.trim();
        if line.contains("<PRESET>") {
            in_presets = true;
            continue;
        }
        if !in_presets {
            continue;
        }
        let Some(name) = line.strip_prefix("- ").and_then(|line| line.split_whitespace().next()) else {
            continue;
        };
        if SUPPORTED.contains(&name) {
            presets.push(name.to_owned());
        }
    }
    presets.sort_by_key(|preset| preset_order(preset));
    let mut result = cxx_qt_lib::QStringList::default();
    for preset in presets {
        result.append(qstring(preset));
    }
    result
}

/// Return a compiled-in legal document by name.
fn legal_document_text(name: &str) -> &'static str {
    match name {
        "license" => include_str!("../LICENSE"),
        "notices" => include_str!("../THIRD-PARTY-NOTICES.md"),
        _ => "",
    }
}

impl qobject::ControlCenter {
    fn begin(mut self: Pin<&mut Self>) {
        self.as_mut().set_busy(true);
        self.as_mut().set_error_message(QString::default());
    }

    fn finish(mut self: Pin<&mut Self>, message_key: &str) {
        self.as_mut().set_busy(false);
        self.as_mut().set_status_message(qstring(message_key));
        self.as_mut().set_last_update(qstring(now_label()));
    }

    fn fail(mut self: Pin<&mut Self>, error: impl Into<String>) {
        self.as_mut().set_busy(false);
        self.as_mut().set_connected(false);
        self.as_mut().set_error_message(qstring(error));
        self.as_mut().set_status_message(qstring("backend.error.operation_failed"));
    }

    /// Run blocking backend work off the Qt thread and apply its result on the Qt thread.
    fn run_async<T, W, A>(self: Pin<&mut Self>, work: W, apply: A)
    where
        T: Send + 'static,
        W: FnOnce() -> T + Send + 'static,
        A: FnOnce(Pin<&mut Self>, T) + Send + 'static,
    {
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = work();
            let _ = qt_thread.queue(move |obj| apply(obj, result));
        });
    }

    pub fn refresh_all(mut self: Pin<&mut Self>) {
        self.as_mut().begin();
        self.run_async(
            || {
                use ipc::IpcRequest;
                read_daemon(
                    |client| {
                        let temps = daemon_response(client, IpcRequest::GetTemperatures)
                            .and_then(daemon_temperatures);
                        let fans = daemon_response(client, IpcRequest::GetFansRPM).and_then(daemon_fans);
                        match (temps, fans) {
                            (Ok(temps), Ok(fans)) => Ok((temps, fans)),
                            (Err(error), _) | (_, Err(error)) => Err(error),
                        }
                    },
                    || {
                        Ok((
                            read_sensor_pair("temps", "backend.error.parse_temps")?,
                            read_sensor_pair("fans", "backend.error.parse_fans")?,
                        ))
                    },
                )
            },
            |mut obj, result| match result {
                Ok(((cpu, system), (cpu_fan, gpu_fan))) => {
                    obj.as_mut().set_cpu_temperature(cpu);
                    obj.as_mut().set_system_temperature(system);
                    obj.as_mut().set_cpu_fan_rpm(cpu_fan);
                    obj.as_mut().set_gpu_fan_rpm(gpu_fan);
                    obj.as_mut().set_connected(true);
                    obj.finish("backend.status.monitor_updated");
                }
                Err(error) => obj.fail(error),
            },
        );
    }

    pub fn refresh_info(mut self: Pin<&mut Self>) {
        self.as_mut().begin();
        self.as_mut().run_async(
            || {
                read_daemon(
                    |client| {
                        let info = native_battery_response(daemon_response(
                            client,
                            ipc::IpcRequest::GetBatteryInfo,
                        )?)?;
                        let charge = (info.current_ma >= 0)
                            .then(|| daemon_response(client, ipc::IpcRequest::GetChargeStatus).ok())
                            .flatten()
                            .and_then(|response| match response {
                                ipc::IpcResponse::ChargeStatus(status) => Some(status),
                                _ => None,
                            });
                        Ok(battery_info_ipc(&info, charge.as_ref()))
                    },
                    || run_cli(&["battery"]).map(|output| battery_details(&output).to_owned()),
                )
            },
            |mut obj, output| match output {
                Ok(output) => obj.as_mut().set_battery_info(qstring(output)),
                Err(error) => obj.as_mut().set_battery_info(qstring(battery_details(&error))),
            },
        );
        self.run_async(
            || {
                read_daemon(
                    |client| match daemon_response(client, ipc::IpcRequest::GetSystemState)? {
                        ipc::IpcResponse::SystemInfo(info) => Ok(format!(
                            "{} ({})\nHRAM: 0x{:04X}\nlecoo-ec-daemon v{}",
                            info.chip, info.revision, info.hram_offset, info.daemon_version
                        )),
                        _ => Err("backend.error.unexpected_controller_info".to_owned()),
                    },
                    || run_cli(&["info"]),
                )
            },
            |mut obj, result| match result {
                Ok(output) => {
                    obj.as_mut().set_connected(true);
                    obj.as_mut().set_controller_info(qstring(output));
                    obj.finish("backend.status.info_updated");
                }
                Err(error) => obj.fail(error),
            },
        );
    }

    /// Read the daemon board and version for the About card.
    pub fn refresh_daemon_info(self: Pin<&mut Self>) {
        self.run_async(
            || {
                read_daemon(
                    |client| match daemon_response(
                        client,
                        ipc::IpcRequest::DaemonCommand(ipc::DaemonCommand::GetCapabilities),
                    )? {
                        ipc::IpcResponse::Capabilities(caps) => Ok((caps.board, caps.daemon_version)),
                        _ => Err("backend.error.unexpected_capabilities".to_owned()),
                    },
                    || {
                        let output = run_cli(&["info"])?;
                        Ok((output, String::new()))
                    },
                )
            },
            |mut obj, result| {
                if let Ok((board, version)) = result {
                    obj.as_mut().set_daemon_board(qstring(board));
                    obj.as_mut().set_daemon_version(qstring(version));
                }
            },
        );
    }

    /// Return a requested legal document.
    pub fn legal_document(&self, name: &QString) -> QString {
        qstring(legal_document_text(&name.to_string()))
    }

    /// The platform's default UI text font family (nothing is bundled).
    pub fn system_font_family(&self) -> QString {
        qobject::shim_system_font_family()
    }

    /// The platform's default fixed-width font family (license viewer body).
    pub fn fixed_font_family(&self) -> QString {
        qobject::shim_fixed_font_family()
    }

    /// Name of the screen the mouse cursor is on (empty when unknown).
    pub fn cursor_screen_name(&self) -> QString {
        qobject::cursor_screen_name()
    }

    pub fn init_tray(self: Pin<&mut Self>) {
        qobject::tray_init(self.upcast_pin());
    }

    pub fn handle_tray_action(self: Pin<&mut Self>, action: i32) {
        self.tray_action(action);
    }

    pub fn tray_set_menu(
        self: Pin<&mut Self>,
        items: &cxx_qt_lib::QStringList,
        profile_index: i32,
        enabled: bool,
    ) {
        qobject::tray_set_menu(items, profile_index, enabled);
    }

    // Qt platform plugin name, such as "wayland", "xcb" or "offscreen".
    pub fn platform_name(&self) -> QString {
        qobject::platform_name()
    }

    pub fn tray_available(&self) -> bool {
        qobject::tray_available()
    }

    pub fn refresh_power_profile(self: Pin<&mut Self>) {
        self.run_async(
            || {
                read_daemon(
                    |client| {
                        daemon_response(client, ipc::IpcRequest::GetPowerProfile)
                            .and_then(power_profile_from_response)
                    },
                    || {
                        let output = run_cli(&["power"])?;
                        let normalized = output.to_lowercase();
                        match if normalized.contains("performance") {
                            Some("perf")
                        } else if normalized.contains("silent") {
                            Some("silent")
                        } else if normalized.contains("default") {
                            Some("default")
                        } else {
                            None
                        } {
                            Some(profile) => Ok(profile.to_owned()),
                            None => Err("backend.error.unexpected_power_response".to_owned()),
                        }
                    },
                )
            },
            |mut obj, result| {
                if let Ok(profile) = result {
                    obj.as_mut().set_power_profile(qstring(profile));
                };
            },
        );
    }

    /// Whether a queued monitor update still belongs to the current process.
    fn monitor_is_current(&self, generation: u64) -> bool {
        self.monitor.lock().map(|state| state.generation == generation).unwrap_or(false)
    }

    /// Apply enabled temperature curves after a monitor update.
    fn apply_fan_curves(&self) {
        let sensors = [self.cpu_temperature; 2];
        let targets = ["cpu", "gpu"];
        let interval =
            Duration::from_millis(*self.fan_curve_interval.lock().unwrap_or_else(|error| error.into_inner()));
        let mut commands = Vec::new();
        let mut curves = self.fan_curves.lock().unwrap_or_else(|error| error.into_inner());

        for (index, sensor) in sensors.into_iter().enumerate() {
            if sensor <= 0 {
                continue;
            }
            let curve = &mut curves[index];
            if !curve.enabled {
                continue;
            }
            let pwm = curve.pwm_for(sensor);
            if pwm == curve.last_pwm {
                continue;
            }
            let now = Instant::now();
            if let Some(last) = curve.last_applied
                && now.duration_since(last) < interval
            {
                continue;
            }
            curve.last_applied = Some(now);
            curve.last_pwm = pwm;
            commands.push((targets[index], pwm));
        }
        drop(curves);

        for (target, pwm) in commands {
            std::thread::spawn(move || {
                let requests = fan_requests(target, "custom", pwm).unwrap_or_default();
                let pwm = pwm.to_string();
                let _ = daemon_write_or_cli(&requests, &["fan", target, "custom", pwm.as_str()]);
            });
        }
    }

    pub fn set_fan_curve(mut self: Pin<&mut Self>, target: &QString, enabled: bool, points: &QString) {
        let index = match target.to_string().as_str() {
            "cpu" => 0,
            "gpu" => 1,
            _ => return,
        };
        let Some(points) = parse_curve_points(&points.to_string()) else {
            return;
        };

        {
            let mut curves = self.fan_curves.lock().unwrap_or_else(|error| error.into_inner());
            let curve = &mut curves[index];
            curve.enabled = enabled;
            curve.points = points;
            curve.reset();
        }

        if enabled && !self.monitoring {
            self.as_mut().toggle_monitoring(true);
        }
    }

    pub fn set_fan_curve_interval(mut self: Pin<&mut Self>, millis: i32) {
        let millis = millis.clamp(500, 10_000) as u64;
        let changed = {
            let mut interval = self.fan_curve_interval.lock().unwrap_or_else(|error| error.into_inner());
            let changed = *interval != millis;
            *interval = millis;
            changed
        };
        // Restart the monitor so sub-second intervals sample fast enough.
        if changed && self.monitoring {
            self.as_mut().toggle_monitoring(true);
        }
    }

    pub fn toggle_monitoring(mut self: Pin<&mut Self>, enabled: bool) {
        if !enabled {
            let curve_enabled = {
                let curves = self.fan_curves.lock().unwrap_or_else(|error| error.into_inner());
                curves.iter().any(|curve| curve.enabled)
            };
            if curve_enabled {
                self.as_mut().set_status_message(qstring("backend.status.curve_monitoring_required"));
                return;
            }
            {
                let mut state = self.monitor.lock().unwrap_or_else(|error| error.into_inner());
                state.stop();
                state.generation = state.generation.wrapping_add(1);
            }
            self.as_mut().set_monitoring(false);
            self.as_mut().set_status_message(qstring("backend.status.monitor_paused"));
            return;
        }

        let millis = *self.fan_curve_interval.lock().unwrap_or_else(|error| error.into_inner());
        let (generation, monitor) = {
            let mut state = self.monitor.lock().unwrap_or_else(|error| error.into_inner());
            state.stop();
            state.generation = state.generation.wrapping_add(1);
            (state.generation, Arc::clone(&self.monitor))
        };

        let qt_thread = self.qt_thread();
        self.as_mut().set_error_message(QString::default());
        self.as_mut().set_monitoring(true);
        self.as_mut().set_status_message(qstring("backend.status.monitor_started"));

        std::thread::spawn(move || {
            let still_current =
                || monitor.lock().map(|state| state.generation == generation).unwrap_or(false);
            let publish = |cpu: i32, system: i32, cpu_fan: i32, gpu_fan: i32| {
                let _ = qt_thread.queue(move |mut obj| {
                    if !obj.monitor_is_current(generation) {
                        return;
                    }
                    obj.as_mut().set_cpu_temperature(cpu);
                    obj.as_mut().set_system_temperature(system);
                    obj.as_mut().set_cpu_fan_rpm(cpu_fan);
                    obj.as_mut().set_gpu_fan_rpm(gpu_fan);
                    obj.as_mut().set_connected(true);
                    obj.as_mut().set_last_update(qstring(now_label()));
                    obj.apply_fan_curves();
                });
            };
            let fallback = if std::env::var("LECOO_CTRL").is_ok_and(|path| !path.trim().is_empty()) {
                true
            } else {
                match ipc::IpcClient::connect() {
                    Ok(mut client) => loop {
                        if !still_current() {
                            return;
                        }
                        let poll_ms = millis.clamp(1000, u16::MAX as u64) as u16;
                        let request = ipc::IpcRequest::GetMetrics { live: Some(poll_ms), since: None };
                        match client.request(&request) {
                            Ok(ipc::IpcResponse::Metrics(metrics)) => {
                                if let Some(sample) = metrics.last {
                                    publish(
                                        sample.cpu_c.into(),
                                        sample.sys_c.into(),
                                        sample.cpu_fan_rpm.into(),
                                        sample.gpu_fan_rpm.into(),
                                    );
                                }
                                std::thread::sleep(Duration::from_millis(1000));
                            }
                            Ok(ipc::IpcResponse::Error(error))
                                if error.code == ipc::ErrorCode::UnsupportedRequest =>
                            {
                                break true;
                            }
                            _ => break true,
                        }
                    },
                    Err(_) => true,
                }
            };

            if !fallback || !still_current() {
                return;
            }

            let rate = if millis < 1000 { format!("{:.1}", millis as f64 / 1000.0) } else { "1".to_owned() };
            let mut child = match spawn_cli(&["monitoring", "--rate", &rate]) {
                Ok(child) => child,
                Err(error) => {
                    let message = if error.kind() == io::ErrorKind::NotFound {
                        "backend.error.cli_unavailable".to_owned()
                    } else {
                        error.to_string()
                    };
                    let _ = qt_thread.queue(move |mut obj| {
                        if obj.monitor_is_current(generation) {
                            obj.as_mut().set_monitoring(false);
                            obj.as_mut().set_error_message(qstring(message));
                        }
                    });
                    return;
                }
            };
            let (Some(stdout), stderr) = (child.stdout.take(), child.stderr.take()) else {
                let _ = child.kill();
                let _ = child.wait();
                return;
            };
            let process = Arc::new(MonitorProcess::new(child));
            {
                let mut state = monitor.lock().unwrap_or_else(|error| error.into_inner());
                if state.generation != generation {
                    process.stop();
                    return;
                }
                state.process = Some(Arc::clone(&process));
            }
            let stderr = stderr.map(|stderr| std::thread::spawn(move || read_stderr(stderr)));
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                if !still_current() {
                    break;
                }
                match read_monitor_line(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        if let Some((cpu, system, cpu_fan, gpu_fan)) = monitoring_values(&text) {
                            publish(cpu, system, cpu_fan, gpu_fan);
                        }
                    }
                    Err(_) => break,
                }
            }
            {
                let mut child = process.child.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(mut child) = child.take() {
                    let _ = child.wait();
                }
            }
            if !still_current() {
                return;
            }
            let detail = stderr.and_then(|handle| handle.join().ok()).unwrap_or_default();
            let message = if detail.is_empty() { "backend.error.monitor_stopped".to_owned() } else { detail };
            let _ = qt_thread.queue(move |mut obj| {
                if !obj.monitor_is_current(generation) {
                    return;
                }
                obj.as_mut().set_monitoring(false);
                obj.as_mut().set_connected(false);
                obj.as_mut().set_error_message(qstring(message));
                obj.as_mut().set_status_message(qstring("backend.error.monitor_stopped"));
                obj.as_mut().set_last_update(qstring(now_label()));
            });
        });
    }

    pub fn apply_power_profile(mut self: Pin<&mut Self>, profile: &QString) {
        let profile = String::from(profile);
        if !matches!(profile.as_str(), "silent" | "default" | "perf") {
            self.fail("backend.error.unsupported_power_profile");
            return;
        }
        self.as_mut().begin();
        let profile_arg = profile.clone();
        self.run_async(
            move || {
                let Some(request) = power_request(&profile_arg) else {
                    return (Err("backend.error.unsupported_power_profile".to_owned()), Ok(()));
                };
                let args = ["power", profile_arg.as_str()];
                let firmware = daemon_write_or_cli(&[request], &args);
                // Never move the system power mode when the firmware rejected
                if firmware.is_err() {
                    return (firmware, Ok(()));
                }
                let overlay = match crate::power::Overlay::for_profile(&profile_arg) {
                    Some(wanted) => match crate::power::effective_overlay() {
                        Ok(Some(current)) if current == wanted => Ok(()),
                        _ => crate::power::set_overlay(wanted),
                    },
                    None => Ok(()),
                };
                (firmware, overlay)
            },
            move |mut obj, (firmware, overlay)| match firmware {
                Err(error) => obj.fail(error),
                Ok(_) => {
                    obj.as_mut().set_power_profile(qstring(profile));
                    obj.apply_overlay_result(overlay, "backend.status.power_applied");
                }
            },
        );
    }

    /// Record the outcome of a system power-mode write.
    fn apply_overlay_result(
        mut self: Pin<&mut Self>,
        overlay: Result<(), crate::power::OverlayError>,
        success_key: &str,
    ) {
        match overlay {
            Ok(()) => self.finish(success_key),
            Err(error) => {
                self.as_mut().set_busy(false);
                self.as_mut().set_error_message(qstring(error.to_string()));
                self.as_mut().set_status_message(qstring(error.message_key()));
                self.as_mut().set_last_update(qstring(now_label()));
            }
        }
    }

    pub fn apply_fan_mode(mut self: Pin<&mut Self>, target: &QString, mode: &QString, pwm: i32) {
        let target = String::from(target);
        let mode = String::from(mode);
        if !matches!(target.as_str(), "cpu" | "gpu" | "both")
            || !matches!(mode.as_str(), "auto" | "full" | "turbo" | "custom")
        {
            self.fail("backend.error.unsupported_fan_mode");
            return;
        }

        self.as_mut().begin();
        let pwm_text = pwm.clamp(0, FAN_PWM_MAX).to_string();
        let target_arg = target.clone();
        let mode_arg = mode.clone();
        self.run_async(
            move || {
                let requests = fan_requests(&target_arg, &mode_arg, pwm)
                    .ok_or_else(|| "backend.error.unsupported_fan_mode".to_owned())?;
                let args = if mode_arg == "custom" {
                    vec!["fan", target_arg.as_str(), mode_arg.as_str(), pwm_text.as_str()]
                } else {
                    vec!["fan", target_arg.as_str(), mode_arg.as_str()]
                };
                daemon_write_or_cli(&requests, &args)
            },
            move |obj, result| match result {
                Ok(_) => {
                    let status_key = match target.as_str() {
                        "cpu" => "backend.status.fan_cpu_applied",
                        "gpu" => "backend.status.fan_gpu_applied",
                        _ => "backend.status.fan_both_applied",
                    };
                    obj.finish(status_key);
                }
                Err(error) => obj.fail(error),
            },
        );
    }

    pub fn refresh_charge(mut self: Pin<&mut Self>) {
        if self.charge_busy {
            return;
        }
        self.as_mut().set_charge_busy(true);
        self.run_async(
            || {
                read_daemon(
                    |client| {
                        let caps = match daemon_response(
                            client,
                            ipc::IpcRequest::DaemonCommand(ipc::DaemonCommand::GetCapabilities),
                        )? {
                            ipc::IpcResponse::Capabilities(caps) => caps,
                            _ => return Err("backend.error.unexpected_capabilities".to_owned()),
                        };
                        let status = match daemon_response(client, ipc::IpcRequest::GetChargeStatus)? {
                            ipc::IpcResponse::ChargeStatus(status) => status,
                            _ => return Err("backend.error.unexpected_charge_status".to_owned()),
                        };
                        Ok((
                            charge_presets_from_caps(&caps),
                            caps.charge.custom_range.is_some(),
                            charge_status_summary(&status),
                        ))
                    },
                    || {
                        let help = run_cli(&["charge", "--help"]).unwrap_or_default();
                        let status = run_cli(&["charge"]).unwrap_or_default();
                        Ok((
                            charge_presets(&help),
                            supports_charge_range(&help),
                            battery_details(&status).to_owned(),
                        ))
                    },
                )
            },
            |mut obj, result| {
                match result {
                    Ok((presets, supports_custom, status)) => {
                        obj.as_mut().set_charge_presets(presets);
                        obj.as_mut().set_custom_charge_supported(supports_custom);
                        obj.as_mut().set_charge_status(qstring(status));
                    }
                    Err(error) => obj.as_mut().set_error_message(qstring(error)),
                }
                obj.set_charge_busy(false);
            },
        );
    }

    pub fn apply_charge_preset(self: Pin<&mut Self>, preset: &QString) {
        let preset = String::from(preset);
        if !matches!(preset.as_str(), "full" | "high" | "balanced" | "lifespan" | "desk" | "hold" | "freeze")
        {
            self.fail("backend.error.unsupported_charge_mode");
            return;
        }
        self.apply_charge(ChargeAction::Preset(preset));
    }

    pub fn apply_charge_range(mut self: Pin<&mut Self>, min: i32, max: i32) {
        if !self.custom_charge_supported {
            self.as_mut().set_error_message(qstring("charge.invalidRange"));
            return;
        }
        let Some([_, min, _, max]) = charge_range_args(min, max) else {
            self.as_mut().set_error_message(qstring("charge.invalidRange"));
            return;
        };
        self.apply_charge(ChargeAction::Range(
            min.parse().unwrap_or_default(),
            max.parse().unwrap_or_default(),
        ));
    }

    fn apply_charge(mut self: Pin<&mut Self>, action: ChargeAction) {
        if self.charge_busy {
            return;
        }
        self.as_mut().set_charge_busy(true);
        self.as_mut().begin();
        self.run_async(
            move || {
                let legacy_args = match &action {
                    ChargeAction::Preset(preset) => vec![preset.clone()],
                    ChargeAction::Range(min, max) => {
                        vec!["--min".into(), min.to_string(), "--max".into(), max.to_string()]
                    }
                };
                apply_charge_native_or_cli(action, legacy_args)
            },
            |mut obj, result| {
                obj.as_mut().set_charge_busy(false);
                match result {
                    Ok(output) => {
                        obj.as_mut().set_charge_status(qstring(battery_details(&output)));
                        obj.finish("backend.status.charge_applied");
                    }
                    Err(error) => {
                        obj.as_mut().set_error_message(qstring(error));
                        obj.finish("backend.error.operation_failed");
                    }
                }
            },
        );
    }

    pub fn apply_keyboard_level(self: Pin<&mut Self>, level: i32) {
        let mode = match level.clamp(0, 3) {
            0 => "off",
            1 => "low",
            2 => "medium",
            _ => "high",
        };
        let mode = mode.to_owned();
        self.apply_keyboard_mode(&qstring(mode), 0);
    }

    pub fn apply_keyboard_mode(mut self: Pin<&mut Self>, mode: &QString, pwm: i32) {
        let mode = String::from(mode);
        if !matches!(mode.as_str(), "off" | "low" | "medium" | "high" | "custom") {
            self.fail("backend.error.unsupported_keyboard_mode");
            return;
        }
        self.as_mut().begin();
        let pwm_text = pwm.clamp(0, 255).to_string();
        let mode_arg = mode.clone();
        self.run_async(
            move || {
                let Some(request) = keyboard_request(&mode_arg, pwm) else {
                    return Err("backend.error.unsupported_keyboard_mode".to_owned());
                };
                let args = if mode_arg == "custom" {
                    vec!["kbd", mode_arg.as_str(), pwm_text.as_str()]
                } else {
                    vec!["kbd", mode_arg.as_str()]
                };
                daemon_write_or_cli(&[request], &args)
            },
            |obj, result| match result {
                Ok(_) => obj.finish("backend.status.keyboard_applied"),
                Err(error) => obj.fail(error),
            },
        );
    }

    pub fn led_presets(&self) -> cxx_qt_lib::QStringList {
        let mut presets = cxx_qt_lib::QStringList::default();
        for preset in LED_PRESETS {
            presets.append(qstring(*preset));
        }
        presets
    }

    pub fn apply_led_mode(
        mut self: Pin<&mut Self>,
        mode: &QString,
        value: i32,
        preset: &QString,
        sleep: bool,
    ) {
        let mode = mode.to_string();
        let preset = preset.to_string();
        let Some(args) = led_args(&mode, value, &preset, sleep) else {
            self.as_mut().set_error_message(qstring("backend.error.unsupported_led_mode"));
            return;
        };
        self.as_mut().begin();
        self.run_async(
            move || {
                let Some(request) = led_request(&mode, value, &preset, sleep) else {
                    let args: Vec<_> = args.iter().map(String::as_str).collect();
                    return run_cli(&args).map(|_| ());
                };
                let args: Vec<_> = args.iter().map(String::as_str).collect();
                daemon_write_or_cli(&[request], &args)
            },
            |mut obj, result| match result {
                Ok(_) => obj.finish("backend.status.led_applied"),
                Err(error) => {
                    obj.as_mut().set_error_message(qstring(error));
                    obj.finish("backend.error.operation_failed");
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_sensor_responses_map_to_gui_properties() {
        assert_eq!(daemon_temperatures(ipc::IpcResponse::Temps { cpu_c: 97, sys_c: 37 }).unwrap(), (97, 37));
        assert_eq!(daemon_fans(ipc::IpcResponse::FanRpm { cpu: 5615, gpu: 5875 }).unwrap(), (5615, 5875));
        assert_eq!(daemon_temperatures(ipc::IpcResponse::Success), Err("backend.error.parse_temps".into()));
        assert_eq!(daemon_fans(ipc::IpcResponse::Success), Err("backend.error.parse_fans".into()));
    }

    #[test]
    fn power_and_fan_modes_map_to_native_requests() {
        assert!(matches!(power_request("silent"), Some(ipc::IpcRequest::SetPowerProfile(_))));
        assert!(power_request("invalid").is_none());
        assert!(matches!(
            fan_requests("cpu", "turbo", 0).unwrap()[0],
            ipc::IpcRequest::SetFanMode {
                fan: lecoo_types::ec_types::FanIndex::Cpu,
                mode: lecoo_types::ec_types::FanMode::Turbo,
            }
        ));
        assert!(matches!(
            fan_requests("both", "custom", 255).unwrap()[1],
            ipc::IpcRequest::SetFanMode {
                fan: lecoo_types::ec_types::FanIndex::Gpu,
                mode: lecoo_types::ec_types::FanMode::Custom(220),
            }
        ));
        assert!(fan_requests("cpu", "invalid", 0).is_none());
    }

    #[test]
    fn keyboard_and_led_modes_map_to_native_requests() {
        assert!(matches!(
            keyboard_request("custom", 255),
            Some(ipc::IpcRequest::SetKeyboardBacklight(
                lecoo_types::ec_types::KeyboardBacklightLevel::Custom(255)
            ))
        ));
        assert!(matches!(
            led_request("breath", 0, "smooth", false),
            Some(ipc::IpcRequest::SetLedMode(lecoo_types::ec_types::PowerLedMode::Animation(
                lecoo_types::ec_types::BreathConfig::SMOOTH
            )))
        ));
        assert!(matches!(
            led_request("breath", 0, "sleep", true),
            Some(ipc::IpcRequest::DaemonCommand(ipc::DaemonCommand::SetLedOnSleep(
                lecoo_types::ec_types::PowerLedMode::Animation(lecoo_types::ec_types::BreathConfig::SLEEP)
            )))
        ));
        assert!(matches!(
            led_request("overheat", 95, "", false),
            Some(ipc::IpcRequest::DaemonCommand(ipc::DaemonCommand::SetOverheatAlert(Some(95))))
        ));
        assert!(led_request("unknown", 0, "", false).is_none());
    }

    #[test]
    fn native_daemon_read_smoke_when_available() {
        let Ok(mut client) = ipc::IpcClient::connect() else {
            return;
        };
        let info = daemon_response(&mut client, ipc::IpcRequest::GetSystemState).unwrap();
        assert!(matches!(info, ipc::IpcResponse::SystemInfo(_)));
        let temps = daemon_response(&mut client, ipc::IpcRequest::GetTemperatures).unwrap();
        let fans = daemon_response(&mut client, ipc::IpcRequest::GetFansRPM).unwrap();
        let (cpu, system) = daemon_temperatures(temps).unwrap();
        let (cpu_fan, gpu_fan) = daemon_fans(fans).unwrap();
        assert!(cpu >= 0 && system >= 0 && cpu_fan >= 0 && gpu_fan >= 0);
    }

    #[test]
    fn charge_range_requires_supported_bounds_and_order() {
        assert_eq!(charge_range_args(20, 100).unwrap(), ["--min", "20", "--max", "100"]);
        for (min, max) in [(19, 80), (70, 101), (80, 70), (80, 80)] {
            assert!(charge_range_args(min, max).is_none());
        }
        assert!(supports_charge_range("  --min <N> --max <N>\n    自定义范围: 20% .. 100%"));
        assert!(!supports_charge_range("Usage: lecoo-ctrl charge [<PRESET> | --min <N> --max <N>]"));
    }

    #[test]
    fn charge_presets_follow_board_specific_help() {
        let n155 = charge_presets(
            "Usage: charge [<PRESET> | --min <N> --max <N>]\n\n<PRESET>\n  - full Full\n  - desk Desk\n  - balanced Balanced\n",
        );
        let n155: Vec<_> = n155.iter().map(|value| value.to_string()).collect();
        assert_eq!(n155, ["full", "balanced", "desk"]);

        let n161 = charge_presets(
            "Usage: charge [<PRESET>]\n\n<PRESET>\n  - full Full\n  - lifespan Lifespan\n  - hold Hold\n",
        );
        let n161: Vec<_> = n161.iter().map(|value| value.to_string()).collect();
        assert_eq!(n161, ["full", "lifespan", "hold"]);
    }

    #[test]
    fn led_arguments_match_upstream_cli() {
        assert_eq!(led_args("auto", 0, "", false).unwrap(), ["led", "auto"]);
        assert_eq!(led_args("breath", 0, "smooth", false).unwrap(), ["led", "breath", "smooth"]);
        assert_eq!(led_args("custom", 255, "", true).unwrap(), ["led", "sleep", "custom", "255"]);
        assert_eq!(led_args("breath", 0, "sleep", true).unwrap(), ["led", "sleep", "breath", "sleep"]);
        assert_eq!(
            led_args("flash", 1500, "alert", false).unwrap(),
            ["led", "flash", "alert", "--for", "1.500"]
        );
        assert_eq!(led_args("overheat", 95, "", false).unwrap(), ["daemon", "overheat", "95"]);
        assert_eq!(led_args("overheat", 0, "", false).unwrap(), ["daemon", "overheat", "off"]);
        assert!(led_args("breath", 0, "--help", false).is_none());
        assert!(led_args("flash", 0, "alert", false).is_none());
        assert!(led_args("flash", 1500, "alert", true).is_none());
    }

    #[test]
    fn battery_details_omits_only_the_cli_heading() {
        assert_eq!(
            battery_details("🔋 电池:\n   电量: 80%\n   健康度: 100%"),
            "   电量: 80%\n   健康度: 100%"
        );
        assert_eq!(battery_details("🔋 Battery:\r\n   Health: 100%"), "   Health: 100%");
        assert_eq!(battery_details("Health: 100%"), "Health: 100%");
        assert_eq!(battery_details("🔋 Battery:"), "");
    }

    #[test]
    fn monitor_updates_split_on_carriage_returns_and_newlines() {
        let mut reader = BufReader::new(b"banner\n\rCPU: 50\r\n\rlast".as_slice());
        let mut line = Vec::new();

        assert_eq!(read_monitor_line(&mut reader, &mut line).unwrap(), 6);
        assert_eq!(line, b"banner");
        assert_eq!(read_monitor_line(&mut reader, &mut line).unwrap(), 7);
        assert_eq!(line, b"CPU: 50");
        assert_eq!(read_monitor_line(&mut reader, &mut line).unwrap(), 4);
        assert_eq!(line, b"last");
        assert_eq!(read_monitor_line(&mut reader, &mut line).unwrap(), 0);
    }

    #[test]
    fn monitoring_values_ignore_the_banner_and_parse_updates() {
        assert_eq!(monitoring_values("Monitoring enabled. Update rate: 1000 ms"), None);
        assert_eq!(
            monitoring_values("🌡️ CPU: 50°C, Sys: 37°C | 💨 Fans: 2867 RPM (CPU), 2994 RPM (GPU)"),
            Some((50, 37, 2867, 2994))
        );
    }

    #[test]
    fn fan_curve_interpolates_and_clamps() {
        let curve = FanCurve::default();
        assert_eq!(curve.pwm_for(30), 60);
        assert_eq!(curve.pwm_for(40), 60);
        assert_eq!(curve.pwm_for(45), 70);
        assert_eq!(curve.pwm_for(90), 220);
        assert_eq!(curve.pwm_for(100), 220);
    }

    #[test]
    fn fan_curve_points_are_parsed_and_clamped() {
        assert_eq!(parse_curve_points("0, 55, 110, 165, 220, 999"), Some([0, 55, 110, 165, 220, 220]));
        assert_eq!(parse_curve_points("220, 0, 160, 10, 200, 0"), Some([220, 220, 220, 220, 220, 220]));
        assert_eq!(parse_curve_points("1, 2"), None);
    }

    #[test]
    fn legal_documents_are_embedded() {
        let license = legal_document_text("license");
        assert!(license.contains("GNU GENERAL PUBLIC LICENSE"));
        assert!(license.contains("Version 3, 29 June 2007"));

        let notices = legal_document_text("notices");
        assert!(notices.starts_with("# Third-party notices"));
        assert!(notices.contains("GPL-3.0-only"));

        // Unknown documents return an empty string.
        assert_eq!(legal_document_text("does-not-exist"), "");
    }
}
