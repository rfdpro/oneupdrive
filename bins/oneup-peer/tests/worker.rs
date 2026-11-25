use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

fn run(root: &Path, op: &str) -> Result<Value> {
    let mut worker = Command::new(env!("CARGO_BIN_EXE_oneup-receive"))
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    writeln!(worker.stdin.take().context("No worker input")?, "{}", json!({"root": root, "op": op}))?;
    let result = worker.wait_with_output()?;
    ensure!(result.status.success(), "Worker failed: {}", String::from_utf8_lossy(&result.stdout));
    Ok(serde_json::from_slice(&result.stdout)?)
}

#[test]
fn settings_worker_reopens_the_same_recipient_identity() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("transfer");
    let ready = run(&root, "prepare")?;
    let reopened = run(&root, "inspect")?;
    assert_eq!(ready["phase"], "ready");
    assert_eq!(reopened["phase"], "ready");
    assert!(ready["code"].as_str().is_some_and(|code| !code.is_empty()));
    assert_eq!(ready["code"], reopened["code"]);
    assert_eq!(ready["session"], reopened["session"]);
    Ok(())
}
