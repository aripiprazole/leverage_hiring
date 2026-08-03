#[tokio::main]
async fn main() {
    if let Err(error) = meier::run().await {
        if let MeierError::ProcessExit { code, .. } = &error {
            std::process::exit((*code).max(1));
        }
        let body = serde_json::json!({"error": error.to_string()});
        match serde_json::to_string(&body) {
            Ok(encoded) => eprintln!("{encoded}"),
            Err(_) => eprintln!(r#"{{"error":"failed to render error"}}"#),
        }
        std::process::exit(error.exit_code());
    }
}
