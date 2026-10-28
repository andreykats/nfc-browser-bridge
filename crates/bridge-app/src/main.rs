//! NFC Browser Bridge: a menu-bar (macOS) / system-tray (Windows) app that runs the bridge core.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod http;
mod instance;
mod logging;
mod menu;
#[cfg(target_os = "macos")]
mod oslog;
mod supervisor;
mod update;
mod upload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::health::Health;
use bridge_core::protocol::DEFAULT_PORT;
use bridge_core::real::RealPcsc;
use bridge_core::{Bridge, BridgeConfig, BridgeStatus, StartError, VERSION};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::MenuEvent;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use bridge_app::icons::{self, IconState};
use menu::{AppState, MenuModel};
use update::UpdateInfo;
use upload::{UploadError, Uploaded};

/// macOS menu-bar icons are template images, tinted by the system for light/dark menu bars.
const TEMPLATE_ICONS: bool = cfg!(target_os = "macos");
const RETRY_BIND: Duration = Duration::from_secs(5);
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// Far above any real PC/SC call (a full-tag write takes about 1 s, a status wait 1 s).
const STALL_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum UserEvent {
    Status(BridgeStatus),
    Menu(MenuEvent),
    Update(UpdateInfo),
    Uploaded(Result<Uploaded, UploadError>),
    RetryBind,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    bridge: Option<Bridge<RealPcsc>>,
    state: AppState,
    shown: Option<MenuModel>,
}

impl App {
    /// Start the bridge, retrying in the background on failure. Returns false if another NFC Browser
    /// Bridge already runs (caller exits).
    fn start_bridge(&mut self) -> bool {
        let proxy = Mutex::new(self.proxy.clone());
        let listener: bridge_core::StatusListener = Arc::new(move |s: &BridgeStatus| {
            let _ = proxy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send_event(UserEvent::Status(s.clone()));
        });
        let e = match Bridge::start(RealPcsc, BridgeConfig::default(), Some(listener)) {
            Ok(b) => {
                watchdog(b.health());
                self.bridge = Some(b);
                self.state.start_error = None;
                return true;
            }
            Err(e) => e,
        };
        let message = match &e {
            StartError::PortInUse if instance::is_our_bridge(DEFAULT_PORT) => return false,
            StartError::PortInUse => format!("Port {DEFAULT_PORT} in use by another app"),
            StartError::Io(e) => format!("Cannot start: {e}"),
        };
        // Nobody may be around to answer a dialog on a kiosk: show it in the menu and keep trying.
        if self.state.start_error.as_ref() != Some(&message) {
            tracing::warn!(error = %e, "cannot start bridge; retrying");
        }
        self.state.start_error = Some(message);
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(RETRY_BIND);
            let _ = proxy.send_event(UserEvent::RetryBind);
        });
        true
    }

    /// Bring the tray in line with the current state.
    fn refresh(&mut self) {
        let model = MenuModel::from_state(&self.state);
        if self.shown.as_ref() == Some(&model) {
            return;
        }
        if let Some(tray) = &self.tray {
            if self.shown.as_ref().map(|m| m.icon) != Some(model.icon) {
                let _ = set_icon(tray, model.icon);
            }
            match model.build() {
                Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                Err(e) => tracing::error!(error = %e, "cannot build menu"),
            }
            let _ = tray.set_tooltip(Some(&model.tooltip));
        }
        self.shown = Some(model);
    }

    fn create_tray(&mut self) {
        let model = MenuModel::from_state(&self.state);
        let menu = match model.build() {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "cannot build menu");
                return;
            }
        };
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(&model.tooltip);
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon(model.icon));
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon(model.icon));
        match builder.build() {
            Ok(tray) => {
                self.tray = Some(tray);
                self.shown = Some(model);
            }
            Err(e) => tracing::error!(error = %e, "cannot create tray icon"),
        }
    }

    fn on_menu(&mut self, id: &str, control_flow: &mut ControlFlow) {
        match id {
            menu::ID_AUTOSTART => {
                let want = !self.state.autostart;
                match autostart::set_enabled(want) {
                    Ok(()) => tracing::info!(enabled = want, "start at login"),
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot change start at login");
                        dialog("Start at login", &e.to_string());
                    }
                }
                self.state.autostart = autostart::is_enabled();
                // Force a rebuild: the check mark toggled itself on click.
                self.shown = None;
            }
            menu::ID_LOGS => {
                if let Some(dir) = logging::log_dir() {
                    open(&dir.to_string_lossy());
                }
            }
            menu::ID_UPLOAD if !self.state.uploading => {
                let proxy = Mutex::new(self.proxy.clone());
                let spawned = upload::spawn(VERSION, move |r| {
                    let _ = proxy
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .send_event(UserEvent::Uploaded(r));
                });
                match spawned {
                    Ok(()) => self.state.uploading = true,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot start uploading logs");
                        dialog("Upload logs", &format!("Could not upload the logs: {e}"));
                    }
                }
            }
            menu::ID_UPDATE => {
                if let Some(u) = &self.state.update {
                    open(&u.url);
                }
            }
            menu::ID_QUIT => {
                tracing::info!("quit");
                self.tray = None;
                if let Some(b) = self.bridge.take() {
                    b.shutdown();
                }
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    }
}

/// A PC/SC call that never returns cannot be interrupted: exit, and the supervisor starts a fresh
/// bridge.
fn watchdog(health: Arc<Health>) {
    let spawned = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            loop {
                std::thread::sleep(WATCHDOG_INTERVAL);
                if let Some(stall) = health.stalled(STALL_LIMIT) {
                    tracing::error!(%stall, "restarting the bridge");
                    std::process::exit(supervisor::EXIT_STALLED);
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the watchdog");
    }
}

/// Keep macOS from putting the menu-bar app in App Nap, which throttles its timers and priority
/// while it has no visible window. Idle system sleep stays allowed.
#[cfg(target_os = "macos")]
fn keep_out_of_app_nap() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &NSString::from_str("Waiting for NFC tags"),
    );
    // The activity lasts as long as the process.
    std::mem::forget(activity);
}

fn icon(state: IconState) -> Icon {
    let (size, template) = if TEMPLATE_ICONS {
        (44, true)
    } else {
        (32, false)
    };
    Icon::from_rgba(icons::rgba(state, size, template), size, size).expect("valid icon dimensions")
}

fn set_icon(tray: &TrayIcon, state: IconState) -> tray_icon::Result<()> {
    #[cfg(target_os = "macos")]
    return tray.set_icon_templated(Some(icon(state)));
    #[cfg(not(target_os = "macos"))]
    return tray.set_icon(Some(icon(state)));
}

fn dialog(title: &str, message: &str) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title(title)
        .set_description(message)
        .show();
}

/// Open a folder or URL with the system handler.
fn open(target: &str) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(target).spawn();
    #[cfg(windows)]
    let r = std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn();
    if let Err(e) = r {
        tracing::warn!(error = %e, target, "cannot open");
    }
}

fn main() {
    logging::init();
    if !supervisor::is_child() {
        // Returns only if the app cannot run as a child; then it runs here, unsupervised.
        supervisor::run();
    }
    tracing::info!(version = VERSION, "NFC Browser Bridge starting");
    #[cfg(target_os = "macos")]
    if let Some(dir) = logging::log_dir() {
        oslog::spawn(&dir);
    }
    autostart::refresh();
    #[cfg(target_os = "macos")]
    keep_out_of_app_nap();

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // Menu-bar only: no Dock icon, no app menu.
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = event_loop.create_proxy();
    {
        let proxy = Mutex::new(proxy.clone());
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = proxy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send_event(UserEvent::Menu(e));
        }));
    }
    let mut app = App {
        proxy,
        tray: None,
        bridge: None,
        state: AppState {
            autostart: autostart::is_enabled(),
            ..Default::default()
        },
        shown: None,
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The tray must be created once the loop runs (macOS requirement). Explorer restarts on
            // Windows ("TaskbarCreated") are handled inside tray-icon, which re-adds the icon.
            Event::NewEvents(StartCause::Init) => {
                if !app.start_bridge() {
                    tracing::info!("another instance is running; exiting");
                    dialog("NFC Browser Bridge", "NFC Browser Bridge is already running. Look for its icon in the menu bar / system tray.");
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                app.create_tray();
                let proxy = Mutex::new(app.proxy.clone());
                update::spawn(VERSION, move |info| {
                    let _ = proxy.lock().unwrap_or_else(|e| e.into_inner()).send_event(UserEvent::Update(info));
                });
            }
            Event::UserEvent(UserEvent::Status(s)) => {
                app.state.status = Some(s);
                app.refresh();
            }
            Event::UserEvent(UserEvent::Update(u)) => {
                app.state.update = Some(u);
                app.refresh();
            }
            Event::UserEvent(UserEvent::Uploaded(r)) => {
                app.state.uploading = false;
                app.refresh();
                match r {
                    Ok(u) => {
                        tracing::info!(folder = %u.folder, files = u.files, "logs uploaded");
                        dialog("Upload logs", &format!("Uploaded {} log files to:\n{}", u.files, u.folder));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot upload logs");
                        dialog("Upload logs", &format!("Could not upload the logs: {e}"));
                    }
                }
            }
            Event::UserEvent(UserEvent::RetryBind) => {
                if app.bridge.is_none() && !app.start_bridge() {
                    // The other app was a NFC Browser Bridge after all.
                    app.tray = None;
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                app.refresh();
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                app.on_menu(e.id.as_ref(), control_flow);
                if *control_flow != ControlFlow::Exit {
                    app.refresh();
                }
            }
            _ => {}
        }
    });
}
