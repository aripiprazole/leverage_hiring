#[tokio::main]
async fn main() {
    let result: meier::Result<()> = meier::run().await.and_then(|output| {
        if let Some(value) = output {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Ok(())
    });
    if let Err(error) = result {
        if let meier::MeierError::ProcessExit { code, .. } = &error {
            std::process::exit((*code).max(1));
        }
        let body = meier::error_body(&error);
        match serde_json::to_string(&body) {
            Ok(encoded) => eprintln!("{encoded}"),
            Err(render_error) => {
                tracing::error!(%render_error, "failed to render the JSON error response");
                eprintln!(r#"{{"error":"failed to render error"}}"#);
            }
        }
        std::process::exit(error.exit_code());
    }
}
