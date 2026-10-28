//! HTTPS client for the update check and log uploads.

use std::time::Duration;

/// HTTPS through the OS TLS stack (SChannel / Security.framework) and the OS trust store, so
/// corporate TLS-inspecting proxies work and no C crypto library is bundled. `timeout` covers a
/// whole request, body included.
pub fn agent(timeout: Duration) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}
