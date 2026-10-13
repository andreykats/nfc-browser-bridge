//! Topps NFC Bridge: a menu-bar (macOS) / system-tray (Windows) app that runs the bridge core.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod instance;
mod logging;
mod menu;
mod update;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge_core::protocol::DEFAULT_PORT;
use bridge_core::real::RealPcsc;
use bridge_core::{Bridge, BridgeConfig, BridgeStatus, StartError};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::MenuEvent;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use bridge_app::icons::{self, IconState};
use menu::{AppState, MenuModel};
use update::UpdateInfo;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RETRY_BIND: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum UserEvent {
    Status(BridgeStatus),
    Menu(MenuEvent),
    Update(UpdateInfo),
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
    /// Start the bridge. Returns false if another Topps NFC Bridge already runs (caller exits).
    fn start_bridge(&mut self) -> bool {
        let proxy = Mutex::new(self.proxy.clone());
        let listener: bridge_core::StatusListener = Arc::new(move |s: &BridgeStatus| {
            let _ = proxy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send_event(UserEvent::Status(s.clone()));
        });
        match Bridge::start(RealPcsc, BridgeConfig::default(), Some(listener)) {
            Ok(b) => {
                self.bridge = Some(b);
                self.state.port_conflict = false;
                true
            }
            Err(StartError::PortInUse) => {
                if instance::is_our_bridge(DEFAULT_PORT) {
                    return false;
                }
                if !self.state.port_conflict {
                    tracing::warn!(port = DEFAULT_PORT, "port in use by another app; retrying");
                }
                self.state.port_conflict = true;
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(RETRY_BIND);
                    let _ = proxy.send_event(UserEvent::RetryBind);
                });
                true
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot start bridge");
                dialog("Topps NFC Bridge could not start", &e.to_string());
                true
            }
        }
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
        let builder = if cfg!(target_os = "macos") {
            builder.with_icon_templated(icon(model.icon))
        } else {
            builder.with_icon(icon(model.icon))
        };
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

fn icon(state: IconState) -> Icon {
    let (size, template) = if cfg!(target_os = "macos") {
        (44, true)
    } else {
        (32, false)
    };
    Icon::from_rgba(icons::rgba(state, size, template), size, size).expect("valid icon dimensions")
}

fn set_icon(tray: &TrayIcon, state: IconState) -> tray_icon::Result<()> {
    if cfg!(target_os = "macos") {
        tray.set_icon_templated(Some(icon(state)))
    } else {
        tray.set_icon(Some(icon(state)))
    }
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
    let _log_guard = logging::init();
    tracing::info!(version = VERSION, "Topps NFC Bridge starting");
    autostart::refresh();

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
                    dialog("Topps NFC Bridge", "Topps NFC Bridge is already running. Look for its icon in the menu bar / system tray.");
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
            Event::UserEvent(UserEvent::RetryBind) => {
                if app.bridge.is_none() && !app.start_bridge() {
                    // The other app was a Topps NFC Bridge after all.
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
