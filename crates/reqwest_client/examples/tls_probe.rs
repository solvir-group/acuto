//! Compares Acuto's TLS configuration with the same configuration minus the
//! post-quantum key share, against one URL.
//!
//! For diagnosing TLS-intercepting filters. A post-quantum key share makes the
//! ClientHello larger than one packet, and a filter that reads the server name
//! from the first packet alone then cannot find it.
//!
//!     cargo run -p reqwest_client --example tls_probe -- https://host/path

use std::sync::Arc;

use rustls_platform_verifier::BuilderVerifierExt as _;

fn without_post_quantum() -> rustls::ClientConfig {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider
        .kx_groups
        .retain(|group| !format!("{:?}", group.name()).contains("MLKEM"));
    rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_platform_verifier()
        .with_no_client_auth()
}

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://integrate.api.nvidia.com/v1/models".to_string());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let groups: Vec<String> = http_client_tls::tls_config()
        .crypto_provider()
        .kx_groups
        .iter()
        .map(|group| format!("{:?}", group.name()))
        .collect();
    println!("acuto key shares: {}", groups.join(", "));

    let configs: [(&str, rustls::ClientConfig); 2] = [
        ("acuto config", http_client_tls::tls_config()),
        ("without post-quantum", without_post_quantum()),
    ];
    for (label, config) in configs {
        let client = match reqwest::Client::builder()
            .use_preconfigured_tls(config)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                println!("{label}: could not build client: {error}");
                continue;
            }
        };
        match runtime.block_on(async { client.get(&url).send().await }) {
            Ok(response) => println!("{label}: OK  status={}", response.status()),
            Err(error) => {
                let mut cause: &dyn std::error::Error = &error;
                while let Some(inner) = cause.source() {
                    cause = inner;
                }
                println!("{label}: FAILED  {cause}");
            }
        }
    }
}
