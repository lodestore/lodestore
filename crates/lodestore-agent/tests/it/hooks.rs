use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use lodestore_agent::Paths;
use lodestore_agent::config::AgentSettings;
use lodestore_agent::hooks::{
    HookError, HookPhase, HookRun, HookSpec, check_script, list_scripts, run_hook,
};
use rustix::process::{Pid, test_kill_process};
use tokio_util::sync::CancellationToken;

fn hooks_dir(root: &Path) -> PathBuf {
    let dir = root.join("hooks.d");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn run<'a>(dir: &'a Path, phase: HookPhase, status: Option<&'a str>) -> HookRun<'a> {
    HookRun {
        hooks_dir: dir,
        allow_inline: false,
        policy: "etc-and-home",
        job_id: "job-1",
        phase,
        backup_status: status,
    }
}

fn named(name: &str, args: &[&str], timeout_s: u32) -> HookSpec {
    HookSpec {
        script: Some(name.to_string()),
        command: None,
        args: args.iter().map(|a| a.to_string()).collect(),
        timeout_s,
    }
}

fn err_code(result: &lodestore_agent::hooks::HookResult) -> &'static str {
    result.outcome.as_ref().unwrap_err().code()
}

fn alive(pid: i32) -> bool {
    let Some(pid) = Pid::from_raw(pid) else {
        return false;
    };
    if test_kill_process(pid).is_err() {
        return false;
    }
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero()));
    !stat.is_ok_and(|s| s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')))
}

async fn wait_for_pid(file: &Path) -> i32 {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(file)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("hook never wrote its pid");
}

async fn assert_dies(pid: i32) {
    for _ in 0..100 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process {pid} is still running");
}

#[tokio::test]
async fn hooks_runs_named_script_with_env() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    let env_out = root.path().join("env");
    let args_out = root.path().join("args");
    script(
        &dir,
        "dump.sh",
        &format!(
            "/usr/bin/env > {}\nprintf '%s\\n' \"$@\" > {}",
            env_out.display(),
            args_out.display()
        ),
        0o755,
    );
    unsafe { std::env::set_var("LODESTORE_S3_SECRET", "must-not-leak") };

    let hook = named("dump.sh", &["a b;echo x", "second"], 0);
    let cancel = CancellationToken::new();
    let result = run_hook(&run(&dir, HookPhase::Pre, None), &hook, &cancel).await;
    result.outcome.unwrap();

    let env = std::fs::read_to_string(&env_out).unwrap();
    let mut keys: Vec<&str> = env
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .filter(|k| !matches!(*k, "PWD" | "OLDPWD" | "SHLVL" | "_"))
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "LODESTORE_HOOK_PHASE",
            "LODESTORE_JOB_ID",
            "LODESTORE_POLICY",
            "PATH"
        ]
    );
    assert!(env.contains("LODESTORE_HOOK_PHASE=pre\n"));
    assert!(env.contains("LODESTORE_POLICY=etc-and-home\n"));
    assert_eq!(
        std::fs::read_to_string(&args_out).unwrap(),
        "a b;echo x\nsecond\n"
    );

    let result = run_hook(&run(&dir, HookPhase::Post, Some("failed")), &hook, &cancel).await;
    result.outcome.unwrap();
    let env = std::fs::read_to_string(&env_out).unwrap();
    assert!(env.contains("LODESTORE_HOOK_PHASE=post\n"));
    assert!(env.contains("LODESTORE_BACKUP_STATUS=failed\n"));
}

#[test]
fn hooks_rejects_bad_names() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    script(&dir, "good", "true", 0o755);
    script(&dir, ".hidden", "true", 0o755);
    for name in ["../x", "a/b", ".hidden", "", "a..b", "-x"] {
        assert!(
            matches!(check_script(&dir, name), Err(HookError::Rejected { .. })),
            "{name:?}"
        );
    }
    assert!(matches!(
        check_script(&dir, "absent"),
        Err(HookError::Missing { .. })
    ));
    assert_eq!(
        check_script(&dir, "absent").unwrap_err().code(),
        "hook_missing"
    );
    assert_eq!(check_script(&dir, "good").unwrap(), dir.join("good"));
}

#[test]
fn hooks_rejects_writable_or_symlink() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    script(&dir, "good", "true", 0o755);
    script(&dir, "group-writable", "true", 0o775);
    script(&dir, "other-writable", "true", 0o757);
    script(&dir, "not-executable", "true", 0o644);
    symlink(dir.join("good"), dir.join("link")).unwrap();
    std::fs::create_dir(dir.join("subdir")).unwrap();

    check_script(&dir, "good").unwrap();
    for name in [
        "group-writable",
        "other-writable",
        "not-executable",
        "link",
        "subdir",
    ] {
        let err = check_script(&dir, name).unwrap_err();
        assert_eq!(err.code(), "hook_rejected", "{name}: {err}");
    }

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert_eq!(
        check_script(&dir, "good").unwrap_err().code(),
        "hook_rejected"
    );
}

#[test]
#[ignore = "needs root"]
fn hooks_rejects_foreign_owner() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    let path = script(&dir, "good", "true", 0o755);
    check_script(&dir, "good").unwrap();
    rustix::fs::chown(
        &path,
        Some(rustix::fs::Uid::from_raw(1234)),
        Some(rustix::fs::Gid::from_raw(1234)),
    )
    .unwrap();
    assert_eq!(
        check_script(&dir, "good").unwrap_err().code(),
        "hook_rejected"
    );
}

#[tokio::test]
async fn hooks_timeout_kills_group() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    let pid_file = root.path().join("pid");
    script(
        &dir,
        "slow",
        &format!("sleep 600 &\necho $! > {}\nwait", pid_file.display()),
        0o755,
    );
    let result = run_hook(
        &run(&dir, HookPhase::Pre, None),
        &named("slow", &[], 3),
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result.outcome,
        Err(HookError::Timeout { timeout_s: 3, .. })
    ));
    assert_eq!(err_code(&result), "hook_timeout");
    assert_dies(wait_for_pid(&pid_file).await).await;
}

#[tokio::test]
async fn hooks_failure_reports_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    script(&dir, "bad", "echo oops\necho err >&2\nexit 3", 0o755);
    let result = run_hook(
        &run(&dir, HookPhase::Pre, None),
        &named("bad", &[], 0),
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result.outcome,
        Err(HookError::Failed { code: Some(3), .. })
    ));
    assert_eq!(result.exit_code, Some(3));
    let output = String::from_utf8(result.output).unwrap();
    assert!(
        output.contains("oops") && output.contains("err"),
        "{output}"
    );
    assert!(!result.truncated);
}

#[tokio::test]
async fn hooks_output_tail_is_capped() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    script(
        &dir,
        "noisy",
        "head -c 204800 /dev/zero | tr '\\0' x\necho end",
        0o755,
    );
    let result = run_hook(
        &run(&dir, HookPhase::Pre, None),
        &named("noisy", &[], 0),
        &CancellationToken::new(),
    )
    .await;
    result.outcome.unwrap();
    assert!(result.truncated);
    assert!(result.output.len() <= 65536);
    assert!(result.output.ends_with(b"xend\n"));
}

#[tokio::test]
async fn hooks_cancel_kills_group() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    let pid_file = root.path().join("pid");
    script(
        &dir,
        "slow",
        &format!("sleep 600 &\necho $! > {}\nwait", pid_file.display()),
        0o755,
    );
    let cancel = CancellationToken::new();
    let hook = named("slow", &[], 0);
    let run_ctx = run(&dir, HookPhase::Pre, None);
    let (result, pid) = tokio::join!(run_hook(&run_ctx, &hook, &cancel), async {
        let pid = wait_for_pid(&pid_file).await;
        cancel.cancel();
        pid
    });
    assert!(matches!(result.outcome, Err(HookError::Cancelled)));
    assert_dies(pid).await;
}

#[tokio::test]
async fn hooks_inline_needs_flag() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    let inline = HookSpec {
        script: None,
        command: Some("echo \"hi $1\"".to_string()),
        args: vec!["there".to_string()],
        timeout_s: 0,
    };
    let cancel = CancellationToken::new();
    let result = run_hook(&run(&dir, HookPhase::Pre, None), &inline, &cancel).await;
    assert!(matches!(result.outcome, Err(HookError::InlineNotAllowed)));
    assert_eq!(err_code(&result), "inline_not_allowed");
    assert!(result.output.is_empty());

    let mut allowed = run(&dir, HookPhase::Pre, None);
    allowed.allow_inline = true;
    let result = run_hook(&allowed, &inline, &cancel).await;
    result.outcome.unwrap();
    assert_eq!(result.output, b"hi there\n");

    let both = HookSpec {
        script: Some("x".to_string()),
        ..inline
    };
    let result = run_hook(&allowed, &both, &cancel).await;
    assert_eq!(err_code(&result), "hook_rejected");
    let neither = HookSpec::default();
    let result = run_hook(&allowed, &neither, &cancel).await;
    assert_eq!(err_code(&result), "hook_rejected");
}

#[test]
fn hooks_list_scripts_reports_ok() {
    let root = tempfile::tempdir().unwrap();
    let dir = hooks_dir(root.path());
    script(&dir, "b", "true", 0o775);
    script(&dir, "a", "true", 0o755);
    script(&dir, ".hidden", "true", 0o755);
    assert_eq!(
        list_scripts(&dir),
        [("a".to_string(), true), ("b".to_string(), false)]
    );
    assert!(list_scripts(&root.path().join("absent")).is_empty());
}

#[test]
fn config_settings_need_safe_file() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_dir: root.path().to_path_buf(),
        state_dir: root.path().join("var"),
    };
    let write = |text: &str, mode: u32| {
        std::fs::write(paths.agent_yaml(), text).unwrap();
        std::fs::set_permissions(paths.agent_yaml(), std::fs::Permissions::from_mode(mode))
            .unwrap();
        AgentSettings::load(&paths).hooks.allow_inline
    };

    assert!(!AgentSettings::load(&paths).hooks.allow_inline);
    assert!(write("hooks:\n  allow_inline: true\n", 0o644));
    assert!(!write("hooks:\n  allow_inline: true\n", 0o666));
    assert!(!write("hooks:\n  allow_inline: true\n", 0o664));
    assert!(!write("hooks: [: nope", 0o644));
    assert!(!write("hooks:\n  allow_inline: false\n", 0o644));
    assert!(!write("{}\n", 0o644));

    let real = root.path().join("real.yaml");
    std::fs::write(&real, "hooks:\n  allow_inline: true\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(paths.agent_yaml()).unwrap();
    symlink(&real, paths.agent_yaml()).unwrap();
    assert!(!AgentSettings::load(&paths).hooks.allow_inline);
}
