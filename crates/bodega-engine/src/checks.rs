//! Running deterministic checks (build, lint, tests) in a workspace.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use bodega_config::Check;
use bodega_core::CheckResult;
use tokio::process::Command;

/// How much of a check's output is kept (and fed back to agents).
const TAIL_LINES: usize = 60;
const TAIL_BYTES: usize = 6_000;

/// Runs one check with `sh -c` in `dir`. A check that cannot start or times
/// out counts as failed; its process group is killed on timeout.
pub async fn run_check(dir: &Path, check: &Check, name_prefix: &str) -> CheckResult {
    let started = Instant::now();
    let name = format!("{name_prefix}{}", check.name);
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&check.command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return CheckResult {
                name,
                command: check.command.clone(),
                passed: false,
                exit_code: None,
                duration_ms: 0,
                output_tail: format!("could not start the check: {e}"),
            };
        }
    };
    let pid = child.id();
    let timeout = Duration::from_secs(check.timeout_secs);
    let (passed, exit_code, output) =
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&stderr);
                }
                (out.status.success(), out.status.code(), text)
            }
            Ok(Err(e)) => (false, None, format!("the check failed to run: {e}")),
            Err(_) => {
                kill_group(pid);
                (
                    false,
                    None,
                    format!("timed out after {}s", check.timeout_secs),
                )
            }
        };
    CheckResult {
        name,
        command: check.command.clone(),
        passed,
        exit_code,
        duration_ms: started.elapsed().as_millis() as u64,
        output_tail: tail(&output),
    }
}

/// Runs checks in order, stopping at the first failure (later checks usually
/// depend on earlier ones, e.g. tests on the build).
pub async fn run_checks(dir: &Path, checks: &[Check], name_prefix: &str) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(checks.len());
    for check in checks {
        let result = run_check(dir, check, name_prefix).await;
        let passed = result.passed;
        results.push(result);
        if !passed {
            break;
        }
    }
    results
}

fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// The last lines of `output`, bounded in bytes.
fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    let mut text = lines[start..].join("\n");
    if text.len() > TAIL_BYTES {
        let mut cut = text.len() - TAIL_BYTES;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = format!("…{}", &text[cut..]);
    }
    if start > 0 {
        text = format!("… ({start} earlier lines omitted)\n{text}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(command: &str, timeout_secs: u64) -> Check {
        Check {
            name: "c".into(),
            command: command.into(),
            timeout_secs,
        }
    }

    #[tokio::test]
    async fn reports_pass_fail_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run_check(dir.path(), &check("echo hello", 10), "").await;
        assert!(ok.passed);
        assert_eq!(ok.exit_code, Some(0));
        assert_eq!(ok.output_tail, "hello");

        let bad = run_check(
            dir.path(),
            &check("echo out; echo err >&2; exit 3", 10),
            "x:",
        )
        .await;
        assert!(!bad.passed);
        assert_eq!(bad.exit_code, Some(3));
        assert_eq!(bad.name, "x:c");
        assert!(bad.output_tail.contains("out") && bad.output_tail.contains("err"));
    }

    #[tokio::test]
    async fn times_out() {
        let dir = tempfile::tempdir().unwrap();
        let slow = run_check(dir.path(), &check("sleep 30", 1), "").await;
        assert!(!slow.passed);
        assert_eq!(slow.exit_code, None);
        assert!(slow.output_tail.contains("timed out"));
        assert!(slow.duration_ms < 10_000);
    }

    #[tokio::test]
    async fn stops_at_the_first_failure() {
        let dir = tempfile::tempdir().unwrap();
        let results = run_checks(
            dir.path(),
            &[check("true", 5), check("false", 5), check("true", 5)],
            "",
        )
        .await;
        assert_eq!(results.len(), 2);
        assert!(!results[1].passed);
    }

    #[test]
    fn tail_keeps_the_end() {
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let t = tail(&long);
        assert!(t.starts_with("… (140 earlier lines omitted)"));
        assert!(t.ends_with("line 199"));
    }
}
