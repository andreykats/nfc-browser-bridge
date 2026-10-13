//! `PcscBackend` over the OS smart-card service (WinSCard.dll / PCSC.framework).

use std::ffi::CString;
use std::time::Duration;

use pcsc::{Context, Protocols, ReaderState, Scope, ShareMode, State};

use crate::backend::{
    Disposition, PNP_READER, PcscBackend, PcscCard, PcscContext, PcscError, ReaderWatch, Transmit,
};

fn err(e: pcsc::Error) -> PcscError {
    PcscError(e as u32)
}

fn cstring(name: &str) -> Result<CString, PcscError> {
    CString::new(name).map_err(|_| PcscError::UNKNOWN_READER)
}

pub struct RealPcsc;

impl PcscBackend for RealPcsc {
    type Context = RealContext;

    fn establish(&self) -> Result<RealContext, PcscError> {
        Context::establish(Scope::User)
            .map(RealContext)
            .map_err(err)
    }
}

#[derive(Clone)]
pub struct RealContext(Context);

impl PcscContext for RealContext {
    type Card = RealCard;

    fn list_readers(&self) -> Result<Vec<String>, PcscError> {
        match self.0.list_readers_owned() {
            Ok(names) => Ok(names
                .into_iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect()),
            Err(pcsc::Error::NoReadersAvailable) => Ok(Vec::new()),
            Err(e) => Err(err(e)),
        }
    }

    fn get_status_change(
        &self,
        timeout: Duration,
        readers: &mut [ReaderWatch],
    ) -> Result<(), PcscError> {
        let mut states = Vec::with_capacity(readers.len());
        for w in readers.iter() {
            let name = if w.is_pnp() {
                pcsc::PNP_NOTIFICATION().to_owned()
            } else {
                cstring(&w.name)?
            };
            states.push(ReaderState::new(
                name,
                State::from_bits_retain(w.current_state),
            ));
        }
        self.0
            .get_status_change(timeout, &mut states)
            .map_err(err)?;
        for (w, s) in readers.iter_mut().zip(&states) {
            w.event_state = s.event_state().bits() | (s.event_count() << 16);
            w.atr = s.atr().to_vec();
        }
        Ok(())
    }

    fn cancel(&self) -> Result<(), PcscError> {
        self.0.cancel().map_err(err)
    }

    fn connect(&self, reader: &str) -> Result<RealCard, PcscError> {
        debug_assert_ne!(reader, PNP_READER);
        self.0
            .connect(
                &cstring(reader)?,
                ShareMode::Shared,
                Protocols::T0 | Protocols::T1,
            )
            .map(RealCard)
            .map_err(err)
    }
}

pub struct RealCard(pcsc::Card);

struct Tx<'a>(&'a pcsc::Card);

impl Transmit for Tx<'_> {
    fn transmit(&self, apdu: &[u8]) -> Result<Vec<u8>, PcscError> {
        let mut buf = [0u8; pcsc::MAX_BUFFER_SIZE_EXTENDED];
        self.0
            .transmit(apdu, &mut buf)
            .map(<[u8]>::to_vec)
            .map_err(err)
    }
}

impl PcscCard for RealCard {
    fn transaction<R>(&mut self, f: impl FnOnce(&dyn Transmit) -> R) -> Result<R, PcscError> {
        let tx = self.0.transaction().map_err(err)?;
        let result = f(&Tx(&tx));
        if let Err((_, e)) = tx.end(pcsc::Disposition::LeaveCard) {
            tracing::warn!(pcsc = %err(e), "SCardEndTransaction failed");
        }
        Ok(result)
    }

    fn reconnect_reset(&mut self) -> Result<Vec<u8>, PcscError> {
        self.0
            .reconnect(
                ShareMode::Shared,
                Protocols::T0 | Protocols::T1,
                pcsc::Disposition::ResetCard,
            )
            .map_err(err)?;
        let status = self.0.status2_owned().map_err(err)?;
        Ok(status.atr().to_vec())
    }

    fn disconnect(self, disposition: Disposition) -> Result<(), PcscError> {
        let d = match disposition {
            Disposition::LeaveCard => pcsc::Disposition::LeaveCard,
            Disposition::ResetCard => pcsc::Disposition::ResetCard,
        };
        self.0.disconnect(d).map_err(|(_, e)| err(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::state;

    #[test]
    fn error_codes_match_pcsc_crate() {
        assert_eq!(err(pcsc::Error::Cancelled), PcscError::CANCELLED);
        assert_eq!(err(pcsc::Error::InvalidHandle), PcscError::INVALID_HANDLE);
        assert_eq!(err(pcsc::Error::UnknownReader), PcscError::UNKNOWN_READER);
        assert_eq!(err(pcsc::Error::Timeout), PcscError::TIMEOUT);
        assert_eq!(
            err(pcsc::Error::SharingViolation),
            PcscError::SHARING_VIOLATION
        );
        assert_eq!(err(pcsc::Error::NoSmartcard), PcscError::NO_SMARTCARD);
        assert_eq!(
            err(pcsc::Error::ReaderUnavailable),
            PcscError::READER_UNAVAILABLE
        );
        assert_eq!(err(pcsc::Error::NoService), PcscError::NO_SERVICE);
        assert_eq!(err(pcsc::Error::ServiceStopped), PcscError::SERVICE_STOPPED);
        assert_eq!(
            err(pcsc::Error::NoReadersAvailable),
            PcscError::NO_READERS_AVAILABLE
        );
        assert_eq!(err(pcsc::Error::RemovedCard), PcscError::REMOVED_CARD);
        assert_eq!(
            err(pcsc::Error::UnresponsiveCard),
            PcscError::UNRESPONSIVE_CARD
        );
        assert_eq!(err(pcsc::Error::ResetCard), PcscError::RESET_CARD);
    }

    #[test]
    fn state_bits_match_pcsc_crate() {
        assert_eq!(State::CHANGED.bits(), state::CHANGED);
        assert_eq!(State::UNKNOWN.bits(), state::UNKNOWN);
        assert_eq!(State::UNAVAILABLE.bits(), state::UNAVAILABLE);
        assert_eq!(State::EMPTY.bits(), state::EMPTY);
        assert_eq!(State::PRESENT.bits(), state::PRESENT);
        assert_eq!(State::MUTE.bits(), state::MUTE);
    }
}
