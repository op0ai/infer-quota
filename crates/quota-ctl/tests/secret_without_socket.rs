//! `quota-ctl secret` is local-only: it must not resolve the daemon socket or
//! read the daemon config. `backends` only lists names; it reads no secret.

use std::path::PathBuf;
use std::process::Command;

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qctl-secret-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("cfg/quota")).unwrap();
    std::fs::write(dir.join("cfg/quota/config.json"), b"{ malformed").unwrap();
    dir
}

fn quota_ctl(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_quota-ctl"))
        .args(args)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("QUOTA_SOCKET")
        .env_remove("QUOTA_OPENBAO_ADDR")
        .output()
        .unwrap()
}

#[test]
fn coderabbit_secret_commands_need_no_socket_configuration() {
    let dir = scratch_dir();

    let secret = quota_ctl(&dir, &["secret", "backends"]);
    assert!(
        secret.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&secret.stderr)
    );
    assert!(String::from_utf8_lossy(&secret.stdout).contains("keychain"));

    let rpc = quota_ctl(&dir, &["ping"]);
    assert!(!rpc.status.success());
    assert!(String::from_utf8_lossy(&rpc.stderr).contains("malformed config"));
    let _ = std::fs::remove_dir_all(dir);
}
