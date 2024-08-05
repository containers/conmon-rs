#![deny(warnings, clippy::all)]

use std::collections::HashMap;
use std::env::VarError;
use std::process::Command;

use pretty_assertions::assert_eq;
use rand::distributions::Alphanumeric;
use rand::Rng;

use libsystemd::logging::*;

fn random_target(prefix: &str) -> String {
    format!(
        "{}_{}",
        prefix,
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect::<String>()
    )
}

#[derive(Debug, Copy, Clone)]
enum Journal {
    User,
    System,
}

fn read_from_journal(journal: Journal, target: &str) -> Vec<HashMap<String, String>> {
    let stdout = String::from_utf8(
        Command::new("journalctl")
            .arg(match journal {
                Journal::User => "--user",
                Journal::System => "--system",
            })
            .arg("--output=json")
            .arg(format!("TARGET={}", target))
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn main() {
    let env_name = "_TEST_LOG_TARGET";

    // On Github Actions use the system instance for this test, because there's
    // no user instance running apparently.
    let journal_instance = if std::env::var_os("GITHUB_ACTIONS").is_some() {
        Journal::System
    } else {
        Journal::User
    };

    match std::env::var(env_name) {
        Ok(target) => {
            journal_send(
                Priority::Info,
                &format!("connected_to_journal() -> {}", connected_to_journal()),
                vec![("TARGET", &target)].into_iter(),
            )
            .unwrap();
        }
        Err(VarError::NotUnicode(value)) => {
            panic!("Value of ${} not unicode: {:?}", env_name, value);
        }
        Err(VarError::NotPresent) => {
            // Restart this binary under systemd-run and then check the journal for the test result
            let exe = std::env::current_exe().unwrap();
            let target = random_target("connected_to_journal");
            let status = match journal_instance {
                Journal::User => {
                    let mut cmd = Command::new("systemd-run");
                    cmd.arg("--user");
                    cmd
                }
                Journal::System => {
                    let mut cmd = Command::new("sudo");
                    cmd.arg("systemd-run");
                    cmd
                }
            }
            .arg("--description=systemd-journal-logger integration test: journal_stream")
            .arg(format!("--setenv={}={}", env_name, target))
            // Wait until the process exited and unload the entire unit afterwards to
            // leave no state behind
            .arg("--wait")
            .arg("--collect")
            .arg(exe)
            .status()
            .unwrap();

            assert!(status.success());

            let entries = read_from_journal(journal_instance, &target);
            assert_eq!(entries.len(), 1);

            assert_eq!(entries[0]["TARGET"], target);
            assert_eq!(entries[0]["MESSAGE"], "connected_to_journal() -> true");
        }
    }
}
