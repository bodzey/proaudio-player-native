use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run(
    program: &str,
    args: &[&str],
    check: bool,
    timeout_secs: u64,
) -> Result<CommandOutput> {
    let child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("не вдалося запустити {program}: {e}"))?;

    let output = timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("команда {program} перевищила час очікування"))??;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if check && !output.status.success() {
        return Err(anyhow!(
            "команда {} {} завершилася помилкою: {}",
            program,
            args.join(" "),
            if stderr.is_empty() {
                format!("exit {code}")
            } else {
                stderr.clone()
            }
        ));
    }
    Ok(CommandOutput {
        code,
        stdout,
        stderr,
    })
}
