//! The tray menu, derived from bridge status. Rebuilt only when what it shows changes.

use bridge_core::protocol::{DEFAULT_PORT, ReaderKind, ServiceStatus};
use bridge_core::{BridgeStatus, ServerState};
use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};

use crate::update::UpdateInfo;
use bridge_app::icons::IconState;

pub const ID_AUTOSTART: &str = "autostart";
pub const ID_LOGS: &str = "logs";
pub const ID_UPLOAD: &str = "upload";
pub const ID_UPDATE: &str = "update";
pub const ID_QUIT: &str = "quit";

/// Everything the app knows that the menu and icon depend on.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub status: Option<BridgeStatus>,
    /// Why the bridge is not running yet (it keeps retrying).
    pub start_error: Option<String>,
    pub autostart: bool,
    pub update: Option<UpdateInfo>,
    /// "Upload logs" is in progress.
    pub uploading: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    pub header: String,
    pub status: String,
    /// Each reader's name and, for the line under it, its card.
    pub readers: Vec<(String, String)>,
    pub clients: String,
    pub autostart: bool,
    pub update: Option<String>,
    pub uploading: bool,
    pub icon: IconState,
    pub tooltip: String,
}

impl MenuModel {
    pub fn from_state(s: &AppState) -> Self {
        let header = format!("NFC Browser Bridge v{}", env!("CARGO_PKG_VERSION"));
        let mut readers = Vec::new();
        let mut icon = IconState::NoReader;
        let status = if let Some(e) = &s.start_error {
            e.clone()
        } else {
            match &s.status {
                None => "Starting…".to_string(),
                Some(st) if st.server == ServerState::Starting => "Starting…".to_string(),
                Some(st) if st.server == ServerState::Stopped => "Stopped".to_string(),
                Some(st) => {
                    let piccs: Vec<_> = st
                        .readers
                        .iter()
                        .filter(|r| r.kind != ReaderKind::Sam)
                        .collect();
                    for r in &piccs {
                        let card = match &r.card {
                            Some(c) => c.uid.clone().unwrap_or_else(|| "(no UID)".into()),
                            None => "waiting…".to_string(),
                        };
                        readers.push((r.name.clone(), card));
                    }
                    if piccs.iter().any(|r| r.card.is_some()) {
                        icon = IconState::CardPresent;
                    } else if !piccs.is_empty() {
                        icon = IconState::Ready;
                    }
                    match st.service {
                        // Windows stops the smart card service when the last reader is unplugged.
                        ServiceStatus::Unavailable if cfg!(windows) => {
                            "No reader connected".to_string()
                        }
                        ServiceStatus::Unavailable => "Smart card service unavailable".to_string(),
                        ServiceStatus::Ready if piccs.is_empty() => {
                            "No reader connected".to_string()
                        }
                        ServiceStatus::Ready => format!("Ready on port {DEFAULT_PORT}"),
                    }
                }
            }
        };
        let clients = match &s.status {
            Some(st) if !st.clients.is_empty() => format!("Clients: {}", st.clients.join(", ")),
            _ => "Clients: none".to_string(),
        };
        let tooltip = format!("NFC Browser Bridge — {status}");
        Self {
            header,
            status,
            readers,
            clients,
            autostart: s.autostart,
            update: s
                .update
                .as_ref()
                .map(|u| format!("Update available: v{}", u.version)),
            uploading: s.uploading,
            icon,
            tooltip,
        }
    }

    pub fn build(&self) -> tray_icon::menu::Result<Menu> {
        let menu = Menu::new();
        menu.append(&MenuItem::new(&self.header, false, None))?;
        menu.append(&MenuItem::new(&self.status, false, None))?;
        for (name, card) in &self.readers {
            menu.append(&MenuItem::new(format!("    {name}"), false, None))?;
            menu.append(&MenuItem::new(format!("        {card}"), false, None))?;
        }
        menu.append(&MenuItem::new(&self.clients, false, None))?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&CheckMenuItem::with_id(
            ID_AUTOSTART,
            "Start at login",
            true,
            self.autostart,
            None,
        ))?;
        menu.append(&MenuItem::with_id(ID_LOGS, "Open log folder", true, None))?;
        menu.append(&if self.uploading {
            MenuItem::with_id(ID_UPLOAD, "Uploading logs…", false, None)
        } else {
            MenuItem::with_id(ID_UPLOAD, "Upload logs", true, None)
        })?;
        if let Some(u) = &self.update {
            menu.append(&MenuItem::with_id(ID_UPDATE, u, true, None))?;
        }
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&MenuItem::with_id(ID_QUIT, "Quit", true, None))?;
        Ok(menu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_core::protocol::{CardInfo, Reader};

    fn status(readers: Vec<Reader>) -> AppState {
        AppState {
            status: Some(BridgeStatus {
                server: ServerState::Listening { port: DEFAULT_PORT },
                service: ServiceStatus::Ready,
                readers,
                clients: vec!["example.com".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn ready_with_card() {
        let m = MenuModel::from_state(&status(vec![
            Reader {
                name: "ACS ACR1552 1S CL Reader PICC 0".into(),
                kind: ReaderKind::Picc,
                card: Some(CardInfo {
                    uid: Some("04699E22C01C90".into()),
                    atr: "3B".into(),
                }),
            },
            Reader {
                name: "ACS ACR1552 1S CL Reader SAM 0".into(),
                kind: ReaderKind::Sam,
                card: None,
            },
        ]));
        assert_eq!(m.status, "Ready on port 38241");
        assert_eq!(
            m.readers,
            vec![(
                "ACS ACR1552 1S CL Reader PICC 0".to_string(),
                "04699E22C01C90".to_string()
            )]
        );
        assert_eq!(m.clients, "Clients: example.com");
        assert_eq!(m.icon, IconState::CardPresent);
    }

    #[test]
    fn no_reader_and_conflict() {
        let m = MenuModel::from_state(&status(vec![]));
        assert_eq!(m.status, "No reader connected");
        assert_eq!(m.icon, IconState::NoReader);
        let m = MenuModel::from_state(&AppState {
            start_error: Some("Port 38241 in use by another app".into()),
            ..Default::default()
        });
        assert_eq!(m.status, "Port 38241 in use by another app");
    }
}
