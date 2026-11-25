use super::*;

#[test]
fn defaults_are_disabled_and_load_without_a_table_or_row() {
    let conn = Connection::open_in_memory().unwrap();
    let expected = Settings {
        enabled: false,
        server: "https://ntfy.sh".into(),
        topic: String::new(),
        needs_you: true,
        done: true,
        unfinished: true,
    };
    assert_eq!(load(&conn).unwrap(), expected);
    init(&conn).unwrap();
    init(&conn).unwrap();
    assert_eq!(load(&conn).unwrap(), expected);
    assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), expected);
}

#[test]
fn settings_round_trip_and_replace_the_singleton() {
    let conn = Connection::open_in_memory().unwrap();
    let settings = Settings {
        enabled: true,
        server: "http://ntfy.lan:8080".into(),
        topic: "private_topic-123".into(),
        needs_you: false,
        done: false,
        unfinished: true,
    };
    save(&conn, &settings).unwrap();
    assert_eq!(load(&conn).unwrap(), settings);
    save(&conn, &Settings::default()).unwrap();
    assert_eq!(load(&conn).unwrap(), Settings::default());
    conn.execute("UPDATE studio_notify SET settings='{'", [])
        .unwrap();
    assert!(load(&conn).is_err());
}

#[test]
fn server_policy() {
    use ServerProblem::*;
    let mut s = Settings {
        enabled: true,
        ..Settings::default()
    };
    for (server, expected) in [
        ("https://ntfy.sh", None),
        ("https://ntfy.sh/", None),
        ("https://ntfy.example.com:8443", None),
        ("https://user:password@ntfy.sh/", Some(HasCredentials)),
        ("https://user@ntfy.sh/", Some(HasCredentials)),
        ("http://:password@localhost:8443/", Some(HasCredentials)),
        ("https://ntfy.sh/team", Some(HasPath)),
        ("https://ntfy.sh/team/..", Some(HasPath)),
        ("https://ntfy.sh/%2e", Some(HasPath)),
        ("https://ntfy.sh/?", Some(HasPath)),
        ("https://ntfy.sh/#", Some(HasPath)),
        ("http://localhost:8080/base", Some(HasPath)),
        ("http://127.0.0.1", None),
        ("http://127.255.255.254", None),
        ("http://[::1]", None),
        ("http://10.5.1.4", None),
        ("http://172.16.0.1", None),
        ("http://172.31.255.254", None),
        ("http://192.168.1.5", None),
        ("http://100.64.0.1", None),
        ("http://100.101.1.2", None),
        ("http://100.127.255.254", None),
        ("http://ntfy.LOCAL", None),
        ("http://a.b.lan", None),
        ("http://example.com", Some(NeedsHttps)),
        ("http://100.63.255.255", Some(NeedsHttps)),
        ("http://100.128.0.1", Some(NeedsHttps)),
        ("http://172.15.0.1", Some(NeedsHttps)),
        ("http://172.32.0.1", Some(NeedsHttps)),
        ("http://169.254.1.2", Some(NeedsHttps)),
        ("http://[fc00::1]", Some(NeedsHttps)),
        ("http://[::ffff:127.0.0.1]", Some(NeedsHttps)),
        ("http://local", Some(NeedsHttps)),
        ("http://ntfy.lan.example.com", Some(NeedsHttps)),
        ("ftp://x", Some(NeedsHttps)),
        ("", Some(Empty)),
        ("  ", Some(Empty)),
        ("not a url", Some(Invalid)),
        ("https://", Some(Invalid)),
        ("http://[::1", Some(Invalid)),
    ] {
        s.server = server.into();
        assert_eq!(check(&s).server, expected, "{server}");
    }
}

#[test]
fn credentials_cannot_be_saved_and_read_errors_do_not_print_them() {
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    let server = "https://fixture-user:fixture-password@ntfy.sh/";
    let settings = Settings {
        server: server.into(),
        ..Settings::default()
    };
    assert_eq!(
        save(&conn, &settings).unwrap_err().to_string(),
        SERVER_CREDENTIALS_ERROR
    );
    assert_eq!(load(&conn).unwrap(), Settings::default());
    assert_eq!(
        redact_server_credentials(server).as_deref(),
        Some("https://ntfy.sh/")
    );

    let malformed = serde_json::json!({"enabled": server});
    conn.execute(
        "INSERT INTO studio_notify VALUES(1,?1)",
        [malformed.to_string()],
    )
    .unwrap();
    let error = format!("{:#}", load(&conn).unwrap_err());
    assert!(!error.contains("fixture-user") && !error.contains("fixture-password"));
}

#[test]
fn topic_policy_and_warning_flags() {
    let mut s = Settings {
        enabled: true,
        needs_you: false,
        done: false,
        unfinished: false,
        ..Settings::default()
    };
    for (topic, expected, short) in [
        (String::new(), Some(TopicProblem::Empty), true),
        ("a".into(), None, true),
        ("ABCxyz_9-01".into(), None, true),
        ("ABCxyz_9-012".into(), None, false),
        ("a".repeat(64), None, false),
        ("a".repeat(65), Some(TopicProblem::Invalid), false),
        ("a/b".into(), Some(TopicProblem::Invalid), true),
        ("a b".into(), Some(TopicProblem::Invalid), true),
        ("a\n".into(), Some(TopicProblem::Invalid), true),
        ("主题".into(), Some(TopicProblem::Invalid), true),
    ] {
        s.topic = topic;
        let got = check(&s);
        assert_eq!(got.topic, expected, "{:?}", s.topic);
        assert_eq!(got.short_topic, short, "{:?}", s.topic);
        assert!(got.nothing_selected);
    }
    s.done = true;
    assert!(!check(&s).nothing_selected);
    s.enabled = false;
    s.server.clear();
    s.done = false;
    assert_eq!(check(&s), Check::default());
}

#[test]
fn generated_topics_have_the_expected_format_and_differ() {
    let first = random_topic();
    let second = random_topic();
    for candidate in [&first, &second] {
        let suffix = candidate.strip_prefix("sa-").unwrap();
        assert_eq!(suffix.len(), 20);
        assert!(suffix.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
    assert_ne!(first, second);
}
