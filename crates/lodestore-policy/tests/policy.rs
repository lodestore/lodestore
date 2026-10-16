use lodestore_policy::{BackupPolicy, Problem, Selector, parse_yaml, to_yaml, validate};

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
    let p = first_problem(&doc("  paths: [/etc]\n  schedule: daily\n"));
    assert_eq!(p.line, Some(8));
    assert!(p.column.is_some());
    assert_eq!(p.path.as_deref(), Some("spec"));
    assert!(
        p.message.contains("unknown field `schedule`"),
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
