use std::time::Duration;

#[tokio::test]
async fn ngrok_tls_initializes_without_crypto_provider_ambiguity() {
    let mut builder = ngrok::Session::builder();
    builder.authtoken("synthetic-regression-test-token");
    let connect = builder.connect();

    // The regression happens synchronously while the ngrok TLS connector chooses a rustls
    // CryptoProvider. Network/authentication success is irrelevant; reaching the timeout or an
    // ordinary connection error without panicking proves the provider graph is unambiguous.
    let _ = tokio::time::timeout(Duration::from_secs(2), connect).await;
}
