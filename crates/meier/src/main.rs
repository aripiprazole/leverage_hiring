#[tokio::main]
async fn main() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let () = rustls::crypto::ring::default_provider()
            .install_default()
            .unwrap();
    }
    match meier::run().await {
        Ok(Some(value)) => match serde_json::to_string_pretty(&value) {
            Ok(encoded) => println!("{encoded}"),
            Err(render_error) => {
                tracing::error!(%render_error, "failed to render the JSON output");
                eprintln!(r#"{{"error":"failed to render output"}}"#);
                std::process::exit(1);
            }
        },
        Ok(None) => {}
        Err(meier::MeierError::ProcessExit { code, .. }) => {
            std::process::exit(code.max(1));
        }
        Err(error) => {
            match serde_json::to_string(&serde_json::json!({"error": error.to_string()})) {
                Ok(encoded) => eprintln!("{encoded}"),
                Err(render_error) => {
                    tracing::error!(%render_error, "failed to render the JSON error response");
                    eprintln!(r#"{{"error":"failed to render error"}}"#);
                }
            }
            std::process::exit(error.exit_code());
        }
    }
}
