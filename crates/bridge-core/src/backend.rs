//! The PC/SC abstraction. `RealPcsc` wraps the OS service; `FakePcsc` is scriptable for tests.

use std::fmt;
use std::time::Duration;

/// A raw PC/SC status code (`SCARD_E_*`, `SCARD_W_*`). Values are identical on Windows and macOS.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcscError(pub u32);

impl PcscError {
    pub const INTERNAL_ERROR: Self = Self(0x8010_0001);
    pub const CANCELLED: Self = Self(0x8010_0002);
    pub const INVALID_HANDLE: Self = Self(0x8010_0003);
    pub const UNKNOWN_READER: Self = Self(0x8010_0009);
    pub const TIMEOUT: Self = Self(0x8010_000A);
    pub const SHARING_VIOLATION: Self = Self(0x8010_000B);
    pub const NO_SMARTCARD: Self = Self(0x8010_000C);
    pub const PROTO_MISMATCH: Self = Self(0x8010_000F);
    pub const COMM_ERROR: Self = Self(0x8010_0013);
    pub const READER_UNAVAILABLE: Self = Self(0x8010_0017);
    pub const NO_SERVICE: Self = Self(0x8010_001D);
    pub const SERVICE_STOPPED: Self = Self(0x8010_001E);
    pub const NO_READERS_AVAILABLE: Self = Self(0x8010_002E);
    pub const UNSUPPORTED_CARD: Self = Self(0x8010_0065);
    pub const UNRESPONSIVE_CARD: Self = Self(0x8010_0066);
    pub const UNPOWERED_CARD: Self = Self(0x8010_0067);
    pub const RESET_CARD: Self = Self(0x8010_0068);
    pub const REMOVED_CARD: Self = Self(0x8010_0069);

    /// The context is dead: release it, back off and re-establish.
    pub fn is_service_lost(self) -> bool {
        matches!(
            self,
            Self::NO_SERVICE | Self::SERVICE_STOPPED | Self::INVALID_HANDLE
        )
    }

    /// A problem with the card or reader (or its sharing), not with the context. Any other error
    /// means the context may be broken and should not be reused.
    pub fn concerns_card(self) -> bool {
        matches!(
            self,
            Self::TIMEOUT
                | Self::SHARING_VIOLATION
                | Self::NO_SMARTCARD
                | Self::PROTO_MISMATCH
                | Self::COMM_ERROR
                | Self::UNKNOWN_READER
                | Self::READER_UNAVAILABLE
                | Self::UNSUPPORTED_CARD
                | Self::UNRESPONSIVE_CARD
                | Self::UNPOWERED_CARD
                | Self::RESET_CARD
                | Self::REMOVED_CARD
        )
    }
}

impl fmt::Debug for PcscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

impl fmt::Display for PcscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

impl std::error::Error for PcscError {}

/// `SCARD_STATE_*` bits (same values on every platform). The upper 16 bits carry the event count.
pub mod state {
    pub const UNAWARE: u32 = 0x0000;
    pub const CHANGED: u32 = 0x0002;
    pub const UNKNOWN: u32 = 0x0004;
    pub const UNAVAILABLE: u32 = 0x0008;
    pub const EMPTY: u32 = 0x0010;
    pub const PRESENT: u32 = 0x0020;
    pub const MUTE: u32 = 0x0200;
}

/// The special reader name that reports reader arrival/removal.
pub const PNP_READER: &str = r"\\?PnP?\Notification";

/// One entry of an `SCardGetStatusChange` call.
#[derive(Debug, Clone)]
pub struct ReaderWatch {
    pub name: String,
    /// The state we believe the reader is in (raw, round-tripped to PC/SC).
    pub current_state: u32,
    /// The state PC/SC reported in the last call (raw).
    pub event_state: u32,
    pub atr: Vec<u8>,
}

impl ReaderWatch {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            current_state: state::UNAWARE,
            event_state: state::UNAWARE,
            atr: Vec::new(),
        }
    }

    pub fn is_pnp(&self) -> bool {
        self.name == PNP_READER
    }

    pub fn has(&self, bit: u32) -> bool {
        self.event_state & bit != 0
    }

    /// Accept the reported state as known, clearing `CHANGED` for the next call.
    pub fn sync(&mut self) {
        self.current_state = self.event_state & !state::CHANGED;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    LeaveCard,
    ResetCard,
}

pub trait PcscBackend: Send + Sync + 'static {
    type Context: PcscContext;
    fn establish(&self) -> Result<Self::Context, PcscError>;
}

/// A PC/SC context. Clones share the underlying handle, so `cancel` can be called from another
/// thread while one clone blocks in `get_status_change`.
pub trait PcscContext: Clone + Send + Sync + 'static {
    type Card: PcscCard;

    /// Reader names. An empty list (not an error) when no readers are connected.
    fn list_readers(&self) -> Result<Vec<String>, PcscError>;
    fn get_status_change(
        &self,
        timeout: Duration,
        readers: &mut [ReaderWatch],
    ) -> Result<(), PcscError>;
    fn cancel(&self) -> Result<(), PcscError>;
    /// Connect in shared mode with T=0 or T=1.
    fn connect(&self, reader: &str) -> Result<Self::Card, PcscError>;
}

pub trait Transmit {
    /// Send one APDU; returns the full response including the status word.
    fn transmit(&self, apdu: &[u8]) -> Result<Vec<u8>, PcscError>;
}

pub trait PcscCard: Send {
    /// Run `f` between `SCardBeginTransaction` and `SCardEndTransaction(LeaveCard)`.
    /// Fails only if the transaction cannot begin; an end failure is logged, since the
    /// commands in `f` have already executed.
    fn transaction<R>(&mut self, f: impl FnOnce(&dyn Transmit) -> R) -> Result<R, PcscError>;
    /// `SCardReconnect` with `SCARD_RESET_CARD`; returns the new ATR.
    fn reconnect_reset(&mut self) -> Result<Vec<u8>, PcscError>;
    fn disconnect(self, disposition: Disposition) -> Result<(), PcscError>;
}
