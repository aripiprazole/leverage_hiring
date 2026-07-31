use std::{path::PathBuf, process::ExitStatus, time::Duration};

use barbirolli::AuthorizedKey;
use tempfile::TempDir;
use tokio::process::Command;

#[derive(Clone)]
pub struct SshFixture {
    guest: std::net::Ipv4Addr,
    private_key: PathBuf,
}

pub struct SshKeyFixture {
    _temporary: TempDir,
    pub private_key: PathBuf,
    pub public_key: AuthorizedKey,
}

#[derive(Debug)]
pub struct SshOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl SshFixture {
    pub fn new(guest: std::net::Ipv4Addr, private_key: PathBuf) -> Self {
        Self { guest, private_key }
    }

    pub fn with_private_key(&self, private_key: PathBuf) -> Self {
        Self::new(self.guest, private_key)
    }

    pub async fn command(&self, command: &str, inspect: impl FnOnce(&SshOutput)) -> SshOutput {
        self.wait_until_ready().await;
        let output = self.run(command).await;
        assert!(
            output.status.success(),
            "SSH command failed: {}",
            output.stderr
        );
        inspect(&output);
        output
    }

    pub async fn try_command(&self, command: &str) -> SshOutput {
        self.wait_until_ready().await;
        self.run(command).await
    }

    pub async fn start_http_server(&self, port: u16, body: &str) {
        let directory = format!("/tmp/barbirolli-http-{port}");
        let command = format!(
            "mkdir -p {directory}; printf %s {} > {directory}/index.html; \
             nohup python3 -m http.server {port} --bind 0.0.0.0 --directory {directory} \
             < /dev/null > {directory}/server.log 2>&1 &",
            shell_quote(body)
        );
        self.command(&command, |_| {}).await;
    }

    pub async fn http_get(&self, url: &str, inspect: impl FnOnce(&SshOutput)) -> SshOutput {
        let command = curl_command(url);
        self.command(&command, inspect).await
    }

    pub async fn try_http_get(&self, url: &str) -> SshOutput {
        self.try_command(&curl_command(url)).await
    }

    async fn wait_until_ready(&self) {
        let mut last_error = String::new();
        for _ in 0..200 {
            let output = self.run("true").await;
            if output.status.success() {
                return;
            }
            last_error = output.stderr;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("SSH did not become ready: {last_error}");
    }

    async fn run(&self, command: &str) -> SshOutput {
        let output = Command::new("ssh")
            .arg("-i")
            .arg(&self.private_key)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=1",
            ])
            .arg(format!("root@{}", self.guest))
            .arg(command)
            .output()
            .await
            .expect("failed to execute SSH");
        SshOutput {
            status: output.status,
            stdout: String::from_utf8(output.stdout).expect("SSH stdout was not UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("SSH stderr was not UTF-8"),
        }
    }
}

impl SshKeyFixture {
    pub async fn generate() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("barbirolli-ssh-key-")
            .tempdir_in("/var/tmp")
            .expect("failed to create SSH key storage");
        let private_key = temporary.path().join("id_ed25519");
        let output = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&private_key)
            .output()
            .await
            .expect("failed to execute ssh-keygen");
        assert!(
            output.status.success(),
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let public_key = tokio::fs::read_to_string(format!("{}.pub", private_key.display()))
            .await
            .expect("failed to read generated public key")
            .trim()
            .parse()
            .expect("ssh-keygen produced an invalid public key");
        Self {
            _temporary: temporary,
            private_key,
            public_key,
        }
    }
}

fn curl_command(url: &str) -> String {
    format!(
        "curl --fail --silent --show-error --connect-timeout 1 --max-time 2 {}",
        shell_quote(url)
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
