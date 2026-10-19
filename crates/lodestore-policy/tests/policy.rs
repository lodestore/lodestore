use lodestore_policy::{
    BackupPolicy, IoniceClass, Problem, Selector, parse_yaml, to_yaml, validate,
};

const QUICKSTART: &str = r#"apiVersion: lodestore.io/v1alpha1
kind: BackupPolicy
metadata:
  name: etc-and-home
spec:
  repository: main
  selector:
    matchLabels: {}
  paths:
  - /etc
  - /home
  exclude:
  - '**/.cache'
  - '**/node_modules'
"#;

fn doc(spec: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: p\nspec:\n  repository: main\n{spec}"
    )
}

const FULL: &str = "  paths: [/etc]
  schedule:
    cron: 30 2 * * *
    timezone: Europe/Kyiv
    jitter: 10m
    catchUpWindow: 12h
  retention:
    keepLast: 3
    keepWithin: 7d
    keepDaily: 7
    keepWeekly: 4
    keepMonthly: 6
  limits:
    uploadRate: 50MiB/s
    nice: 10
    ioniceClass: best-effort
    ioniceLevel: 4
";

fn with_paths(tail: &str) -> String {
    doc(&format!("  paths: [/etc]\n{tail}"))
}

fn check(tail: &str, path: &str, message: &str) {
    let found = parse_yaml(&with_paths(tail)).expect_err(tail);
    assert_eq!(found.len(), 1, "{tail}: {found:?}");
    assert_eq!(found[0].path.as_deref(), Some(path), "{tail}");
    assert_eq!(found[0].message, message, "{tail}");
}

fn accepted(tail: &str) {
    if let Err(found) = parse_yaml(&with_paths(tail)) {
        panic!("{tail}: {found:?}");
    }
}

#[test]
fn policy_schedule_roundtrip() {
    let policy = parse_yaml(&doc(FULL)).expect("parses");
    let schedule = policy.spec.schedule.as_ref().unwrap();
    assert_eq!(schedule.cron, "30 2 * * *");
    assert_eq!(schedule.timezone.as_deref(), Some("Europe/Kyiv"));
    assert_eq!(
        policy.spec.retention.as_ref().unwrap().keep_monthly,
        Some(6)
    );
    assert_eq!(
        policy.spec.limits.as_ref().unwrap().ionice_class,
        Some(IoniceClass::BestEffort)
    );
    let text = to_yaml(&policy);
    assert!(text.contains("ioniceClass: best-effort"), "{text}");
    let again = parse_yaml(&text).expect("canonical text parses");
    assert_eq!(again, policy);
    assert_eq!(to_yaml(&again), text);
}

#[test]
fn policy_legacy_yaml_unchanged() {
    let policy = parse_yaml(QUICKSTART).unwrap();
    assert!(policy.spec.schedule.is_none());
    assert!(policy.spec.retention.is_none());
    assert!(policy.spec.limits.is_none());
    assert_eq!(to_yaml(&policy), QUICKSTART);
    let minimal = parse_yaml(&with_paths("  schedule:\n    cron: '@daily'\n")).unwrap();
    let schedule = minimal.spec.schedule.unwrap().compile().unwrap();
    assert_eq!(schedule.timezone_name(), "UTC");
    assert_eq!(schedule.jitter().as_secs(), 15 * 60);
    assert_eq!(schedule.catch_up_window().as_secs(), 24 * 3600);
}

#[test]
fn policy_schedule_validation() {
    const FIVE: &str =
        "Use five fields (minute hour day month weekday) or @hourly, @daily, @weekly, @monthly.";
    const DURATION: &str = "Use a duration such as 15m, 24h, 30d or 1y.";
    let sched = |lines: &str| format!("  schedule:\n{lines}");
    check(
        &sched("    cron: 0 30 2 * * *\n"),
        "spec.schedule.cron",
        FIVE,
    );
    check(
        &sched("    cron: 0 0 30 2 * * 2026\n"),
        "spec.schedule.cron",
        FIVE,
    );
    check(&sched("    cron: '@never'\n"), "spec.schedule.cron", FIVE);
    check(&sched("    cron: ''\n"), "spec.schedule.cron", FIVE);
    check(
        &sched("    cron: '@daily'\n    timezone: Mars/Olympus\n"),
        "spec.schedule.timezone",
        "Unknown time zone \"Mars/Olympus\". Use a name such as Europe/Kyiv.",
    );
    for (field, value) in [
        ("jitter", "15x"),
        ("jitter", "25h"),
        ("jitter", "-5m"),
        ("catchUpWindow", "31d"),
        ("catchUpWindow", "30s"),
        ("catchUpWindow", "0m"),
    ] {
        check(
            &sched(&format!("    cron: '@daily'\n    {field}: {value}\n")),
            &format!("spec.schedule.{field}"),
            DURATION,
        );
    }
    for ok in [
        "jitter: 0s",
        "jitter: 24h",
        "catchUpWindow: 1m",
        "catchUpWindow: 30d",
    ] {
        accepted(&sched(&format!("    cron: '@daily'\n    {ok}\n")));
    }
    for alias in ["@hourly", "@daily", "@weekly", "@monthly"] {
        accepted(&sched(&format!("    cron: '{alias}'\n")));
    }
}

#[test]
fn policy_limits_bounds() {
    const NICE: &str = "Enter a whole number from 0 to 19.";
    const LEVEL: &str = "Enter a whole number from 0 to 7.";
    const CLASS: &str = "Set ioniceClass to best-effort to use a level.";
    const RATE: &str = "Use a rate such as 50MiB/s or 800KiB/s.";
    let limits = |lines: &str| format!("  limits:\n{lines}");
    for ok in ["0", "19"] {
        accepted(&limits(&format!("    nice: {ok}\n")));
    }
    for bad in ["20", "-1"] {
        check(
            &limits(&format!("    nice: {bad}\n")),
            "spec.limits.nice",
            NICE,
        );
    }
    for ok in [0, 7] {
        accepted(&limits(&format!(
            "    ioniceClass: best-effort\n    ioniceLevel: {ok}\n"
        )));
    }
    check(
        &limits("    ioniceClass: best-effort\n    ioniceLevel: 8\n"),
        "spec.limits.ioniceLevel",
        LEVEL,
    );
    check(
        &limits("    ioniceLevel: 3\n"),
        "spec.limits.ioniceLevel",
        CLASS,
    );
    check(
        &limits("    ioniceClass: idle\n    ioniceLevel: 3\n"),
        "spec.limits.ioniceLevel",
        CLASS,
    );
    accepted(&limits("    ioniceClass: idle\n"));
    for bad in [
        "1023B/s", "0B/s", "50MB/s", "50MiB", "MiB/s", "-1KiB/s", "1.5MiB/s", "",
    ] {
        check(
            &limits(&format!("    uploadRate: '{bad}'\n")),
            "spec.limits.uploadRate",
            RATE,
        );
    }
    for ok in ["1KiB/s", "800KiB/s", "50MiB/s", "1GiB/s", "1024B/s"] {
        accepted(&limits(&format!("    uploadRate: {ok}\n")));
    }
    assert!(parse_yaml(&with_paths("  limits:\n    ioniceClass: realtime\n")).is_err());
}

#[test]
fn policy_max_runtime_bounds() {
    const RULE: &str = "Use a duration from 1h to 7d, such as 12h.";
    let limits = |value: &str| format!("  limits:\n    maxRuntime: {value}\n");
    for ok in ["1h", "12h", "7d", "168h"] {
        accepted(&limits(ok));
    }
    for bad in ["59m", "8d", "169h", "abc", "0h", "\"\""] {
        check(&limits(bad), "spec.limits.maxRuntime", RULE);
    }
}

#[test]
fn policy_max_runtime_roundtrip() {
    let policy = parse_yaml(&with_paths("  limits:\n    maxRuntime: 2h\n")).unwrap();
    assert_eq!(
        policy.spec.limits.as_ref().unwrap().max_runtime.as_deref(),
        Some("2h")
    );
    let text = to_yaml(&policy);
    assert!(text.contains("maxRuntime: 2h"), "{text}");
    assert_eq!(parse_yaml(&text).unwrap(), policy);
}

#[test]
fn policy_retention_counts() {
    const COUNT: &str = "Enter a whole number of 1 or more.";
    const DURATION: &str = "Use a duration such as 15m, 24h, 30d or 1y.";
    let ret = |lines: &str| format!("  retention:\n{lines}");
    for field in ["keepLast", "keepDaily", "keepWeekly", "keepMonthly"] {
        for bad in ["0", "-3"] {
            check(
                &ret(&format!("    {field}: {bad}\n")),
                &format!("spec.retention.{field}"),
                COUNT,
            );
        }
        accepted(&ret(&format!("    {field}: 1\n")));
    }
    for bad in ["0s", "59s", "1x", "abc"] {
        check(
            &ret(&format!("    keepWithin: {bad}\n")),
            "spec.retention.keepWithin",
            DURATION,
        );
    }
    for ok in ["1m", "30d", "2y", "4w"] {
        accepted(&ret(&format!("    keepWithin: {ok}\n")));
    }
}

#[test]
fn policy_unknown_field_in_schedule() {
    let p = first_problem(&with_paths("  schedule:\n    crn: '@daily'\n"));
    assert_eq!(p.line, Some(9));
    assert!(p.message.contains("unknown field `crn`"), "{}", p.message);
}

fn first_problem(text: &str) -> Problem {
    parse_yaml(text).expect_err("must be rejected").remove(0)
}

fn problem_paths(text: &str) -> Vec<Option<String>> {
    parse_yaml(text)
        .expect_err("must be rejected")
        .into_iter()
        .map(|p| p.path)
        .collect()
}

fn hook_doc(hook: &str) -> String {
    doc(&format!(
        "  paths: [/etc]\n  hooks:\n    before:\n    - {hook}\n"
    ))
}

#[test]
fn policy_quickstart_roundtrip() {
    let policy = parse_yaml(QUICKSTART).expect("quickstart parses");
    assert_eq!(to_yaml(&policy), QUICKSTART);
    assert!(Selector::default().matches("h1", []));
    assert!(Selector::default().matches("h1", [("env", "prod")]));
}

#[test]
fn policy_unknown_field_has_location() {
    let p = first_problem(&doc("  paths: [/etc]\n  retries: 3\n"));
    assert_eq!(p.line, Some(8));
    assert!(p.column.is_some());
    assert_eq!(p.path.as_deref(), Some("spec"));
    assert!(
        p.message.contains("unknown field `retries`"),
        "{}",
        p.message
    );
    assert!(!p.message.contains(" at line "), "{}", p.message);
}

#[test]
fn policy_type_error_has_path() {
    let p = first_problem(&doc("  paths: 5\n"));
    assert_eq!(p.path.as_deref(), Some("spec.paths"));
    assert!(p.line.is_some());
    assert!(!p.message.contains(" at line "), "{}", p.message);
}

#[test]
fn policy_relative_path_rejected() {
    let p = first_problem(&doc("  paths: [etc]\n"));
    assert_eq!(p.path.as_deref(), Some("spec.paths[0]"));
    assert_eq!(p.message, "Enter an absolute path that starts with \"/\".");
    assert!(parse_yaml(&doc("  paths: []\n")).is_err());
    assert!(parse_yaml(&doc("  paths: [/a, /a]\n")).is_err());
}

#[test]
fn policy_hook_rules() {
    let p = first_problem(&hook_doc("{script: ../x}"));
    assert_eq!(p.path.as_deref(), Some("spec.hooks.before[0].script"));
    assert_eq!(
        p.message,
        "Enter a file name from /etc/lodestore/hooks.d, without \"/\"."
    );
    assert!(parse_yaml(&hook_doc("{script: a/b}")).is_err());
    assert!(parse_yaml(&hook_doc("{script: a..b}")).is_err());
    assert!(parse_yaml(&hook_doc(&format!("{{script: {}}}", "a".repeat(256)))).is_err());
    assert!(parse_yaml(&hook_doc(&format!("{{script: {}}}", "a".repeat(255)))).is_ok());
    assert!(parse_yaml(&hook_doc("{script: dump-db, command: x}")).is_err());
    assert!(parse_yaml(&hook_doc("{args: [a]}")).is_err());
    let ok = parse_yaml(&hook_doc("{script: dump-db.sh, args: [app]}")).unwrap();
    assert_eq!(ok.spec.hooks.before[0].timeout_secs(), 3600);
    assert!(parse_yaml(&hook_doc("{command: 'echo hi'}")).is_ok());
}

#[test]
fn policy_timeout_grammar() {
    let p = first_problem(&hook_doc("{script: a, timeout: 90x}"));
    assert_eq!(p.path.as_deref(), Some("spec.hooks.before[0].timeout"));
    assert_eq!(p.message, "Use a duration such as 90s, 30m or 2h.");
    for bad in [
        "25h",
        "1441m",
        "0s",
        "s",
        "1d",
        "-5s",
        "99999999999999999999h",
    ] {
        assert!(
            parse_yaml(&hook_doc(&format!("{{script: a, timeout: {bad}}}"))).is_err(),
            "{bad}"
        );
    }
    let ok = parse_yaml(&hook_doc("{script: a, timeout: 30m}")).unwrap();
    assert_eq!(ok.spec.hooks.before[0].timeout_secs(), 1800);
    let day = parse_yaml(&hook_doc("{script: a, timeout: 24h}")).unwrap();
    assert_eq!(day.spec.hooks.before[0].timeout_secs(), 86400);
}

#[test]
fn policy_name_and_kind_rules() {
    let base = doc("  paths: [/etc]\n");
    for name in ["Etc_Home", "-a", "a-", &"a".repeat(64)] {
        let text = base.replace("name: p\n", &format!("name: {name}\n"));
        assert!(
            problem_paths(&text).contains(&Some("metadata.name".to_string())),
            "{name}"
        );
    }
    let kind = problem_paths(&base.replace("kind: BackupPolicy", "kind: Other"));
    assert_eq!(kind[0].as_deref(), Some("kind"));
    let version = problem_paths(&base.replace("lodestore.io/v1alpha1", "example.com/v1"));
    assert_eq!(version[0].as_deref(), Some("apiVersion"));
}

#[test]
fn policy_bad_exclude_glob() {
    let p = first_problem(&doc("  paths: [/etc]\n  exclude: ['**/ok', '[z-a]']\n"));
    assert_eq!(p.path.as_deref(), Some("spec.exclude[1]"));
}

#[test]
fn policy_selector_label_rules() {
    let long = "k".repeat(64);
    for label in [
        format!("{long}: v"),
        "a/b: v".to_string(),
        "k: 'a b'".to_string(),
    ] {
        let p = first_problem(&doc(&format!(
            "  selector:\n    matchLabels:\n      {label}\n  paths: [/etc]\n"
        )));
        assert_eq!(
            p.path.as_deref(),
            Some("spec.selector.matchLabels"),
            "{label}"
        );
    }
    let policy = parse_yaml(&doc(
        "  selector:\n    matchLabels:\n      env: prod\n      tier: db\n  paths: [/etc]\n",
    ))
    .unwrap();
    let sel = &policy.spec.selector;
    assert!(sel.matches("h1", [("env", "prod"), ("tier", "db"), ("x", "y")]));
    assert!(!sel.matches("h1", [("env", "prod")]));
    assert!(!sel.matches("h1", [("env", "dev"), ("tier", "db")]));
}

#[test]
fn policy_json_object_validates() {
    let json = serde_json::json!({
        "apiVersion": "lodestore.io/v1alpha1",
        "kind": "BackupPolicy",
        "metadata": {"name": "p"},
        "spec": {"repository": "main", "paths": ["rel"]}
    });
    let policy: BackupPolicy = serde_json::from_value(json).unwrap();
    let found = validate(&policy);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path.as_deref(), Some("spec.paths[0]"));
    assert_eq!(found[0].line, None);
}

#[test]
fn policy_selector_host_list() {
    let policy = parse_yaml(&doc("  selector:\n    hosts: [h1, h2]\n  paths: [/etc]\n")).unwrap();
    assert!(validate(&policy).is_empty());
    let sel = &policy.spec.selector;
    assert!(sel.matches("h2", []));
    assert!(!sel.matches("h3", [("env", "prod")]));
    assert!(to_yaml(&policy).contains("hosts:"));

    let both =
        doc("  selector:\n    matchLabels:\n      env: prod\n    hosts: [h1]\n  paths: [/etc]\n");
    assert_eq!(
        problem_paths(&both),
        vec![Some("spec.selector".to_string())]
    );
    let twice = doc("  selector:\n    hosts: [h1, h1]\n  paths: [/etc]\n");
    assert_eq!(
        problem_paths(&twice),
        vec![Some("spec.selector.hosts[1]".to_string())]
    );
    let empty = doc("  selector:\n    hosts: []\n  paths: [/etc]\n");
    assert_eq!(
        problem_paths(&empty),
        vec![Some("spec.selector.hosts".to_string())]
    );
    let bad = doc("  selector:\n    hosts: [\"a/b\"]\n  paths: [/etc]\n");
    assert_eq!(
        problem_paths(&bad),
        vec![Some("spec.selector.hosts[0]".to_string())]
    );
}
