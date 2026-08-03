use std::process::Stdio;

use serde_json::Value;
use tokio::process::Command;

use crate::{MeierError, error::process_exit_code};

fn limactl() -> String {
    std::env::var("LIMACTL").unwrap_or_else(|_| "limactl".to_owned())
}

pub async fn ensure_running(instance: &str) -> Result<(), MeierError> {
    tracing::debug!(%instance, "checking Lima instance state");
    let output = Command::new(limactl())
        .args(["list", "--format", "json", instance])
        .output()
        .await
        .map_err(|source| MeierError::Lima {
            message: format!("could not execute limactl: {source}"),
        })?;
    if !output.status.success() {
        return Err(MeierError::Lima {
            message: format!(
                "instance {instance:?} is unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| MeierError::Lima {
            message: format!("limactl returned invalid JSON: {error}"),
        })?;
    let Some(status) = find_status(&value) else {
        return Err(MeierError::Lima {
            message: format!("instance {instance:?} is unavailable or missing"),
        });
    };
    if !status.eq_ignore_ascii_case("running") {
        return Err(MeierError::Lima {
            message: format!("instance {instance:?} is not running (status: {status})"),
        });
    }
    Ok(())
}

fn find_status(value: &Value) -> Option<&str> {
    match value {
        Value::Array(values) => values.iter().find_map(find_status),
        Value::Object(map) => map
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| map.get("Status").and_then(Value::as_str)),
        _ => None,
    }
}

pub async fn capture(instance: &str, args: &[String]) -> Result<std::process::Output, MeierError> {
    tracing::debug!(%instance, command = ?args.first(), "executing command through Lima");
    Command::new(limactl())
        .arg("shell")
        .arg(instance)
        .arg("--")
        .args(args)
        .output()
        .await
        .map_err(|source| MeierError::Lima {
            message: format!("could not execute limactl shell: {source}"),
        })
}

pub async fn passthrough(instance: &str, args: &[String]) -> Result<(), MeierError> {
    tracing::debug!(%instance, command = ?args.first(), "streaming command through Lima");
    let status = Command::new(limactl())
        .arg("shell")
        .arg(instance)
        .arg("--")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|source| MeierError::Lima {
            message: format!("could not execute limactl shell: {source}"),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(MeierError::ProcessExit {
            program: limactl(),
            code: process_exit_code(status),
        })
    }
}

pub async fn guest_home(instance: &str) -> Result<String, MeierError> {
    let output = capture(
        instance,
        &[
            "sh".to_owned(),
            "-c".to_owned(),
            "printf %s \"$HOME\"".to_owned(),
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(MeierError::Lima {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|error| MeierError::Lima {
            message: format!("guest HOME was not UTF-8: {error}"),
        })
}
