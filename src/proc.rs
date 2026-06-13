use std::process::{Command, Output, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

/// Run a command capturing stdout/stderr with a hard timeout.
/// On timeout the child is killed and Err is returned.
pub fn output_with_timeout(mut cmd: Command, secs: u64) -> anyhow::Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    match child.wait_timeout(Duration::from_secs(secs))? {
        Some(_) => Ok(child.wait_with_output()?),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("command timed out after {secs}s")
        }
    }
}
