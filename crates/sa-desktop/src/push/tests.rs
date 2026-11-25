use super::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Instant;

const TIMEOUT: Duration = Duration::from_secs(3);
const TOPIC: &str = "test-private-topic";

fn finished(state: &str) -> Event {
    Event::Finished {
        ticket: 42,
        state: state.into(),
    }
}

fn selected() -> Settings {
    Settings {
        enabled: true,
        ..Settings::default()
    }
}

#[test]
fn event_copy_priority_and_tags_match_the_spec() {
    for (event, message, priority, tag) in [
        (
            Event::Question { ticket: 42 },
            "工单 #42 需要你回答问题",
            4,
            "question",
        ),
        (
            Event::Approval { ticket: 42 },
            "工单 #42 等待你审批",
            4,
            "raised_hand",
        ),
        (finished("done"), "工单 #42 已完成", 3, "white_check_mark"),
        (finished("failed"), "工单 #42 失败", 4, "x"),
        (finished("partial"), "工单 #42 部分完成", 4, "warning"),
        (
            finished("interrupted"),
            "工单 #42 已中断",
            4,
            "pause_button",
        ),
    ] {
        let push = push_for(&event, &selected()).unwrap();
        assert_eq!(push.title, "Super Agent");
        assert_eq!(push.message, message);
        assert_eq!(push.priority, priority);
        assert_eq!(push.tags, [tag]);
    }
    for state in [
        "cancelled",
        "queued",
        "running",
        "waiting",
        "draining",
        "unknown",
        "",
    ] {
        assert!(push_for(&finished(state), &selected()).is_none(), "{state}");
    }
    let push = test_push();
    assert_eq!(push.title, "Super Agent 测试推送");
    assert_eq!(push.message, "收到这条，说明推送设置正确。");
}

#[test]
fn master_switch_and_category_filters_are_independent() {
    let cases = [
        (Event::Question { ticket: 42 }, [true, false, false]),
        (Event::Approval { ticket: 42 }, [true, false, false]),
        (finished("done"), [false, true, false]),
        (finished("failed"), [false, false, true]),
        (finished("partial"), [false, false, true]),
        (finished("interrupted"), [false, false, true]),
    ];
    for (event, expected) in cases {
        assert!(push_for(&event, &Settings::default()).is_none());
        for (index, expected) in expected.into_iter().enumerate() {
            let s = Settings {
                needs_you: index == 0,
                done: index == 1,
                unfinished: index == 2,
                ..selected()
            };
            assert_eq!(push_for(&event, &s).is_some(), expected);
        }
    }
}

fn stub<T: Send + 'static>(
    respond: impl FnOnce(TcpStream) -> T + Send + 'static,
) -> (String, JoinHandle<T>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        respond(stream)
    });
    (format!("http://{address}"), worker)
}

fn read_publish(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "POST / HTTP/1.1\r\n");
    let mut length = None;
    let mut content_type = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse().unwrap());
        }
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_owned());
        }
    }
    assert_eq!(content_type.as_deref(), Some("application/json"));
    let mut body = vec![0; length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn reply(stream: &mut TcpStream, status: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
}

#[test]
fn send_posts_exact_json_to_the_server_root() {
    let (server, worker) = stub(|mut stream| {
        let body = read_publish(&mut stream);
        reply(&mut stream, "200 OK");
        body
    });
    let push = push_for(&Event::Question { ticket: 42 }, &selected()).unwrap();
    assert_eq!(send(&server, TOPIC, &push, TIMEOUT), Ok(()));
    assert_eq!(
        worker.join().unwrap(),
        json!({
            "topic": TOPIC,
            "title": "Super Agent",
            "message": "工单 #42 需要你回答问题",
            "priority": 4,
            "tags": ["question"],
        })
    );
}

#[test]
fn send_rejects_a_non_root_endpoint_before_opening_a_connection() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let address = server.local_addr().unwrap();
    for suffix in ["/team", "/?topic=team", "/#team"] {
        assert_eq!(
            send(
                &format!("http://{address}{suffix}"),
                TOPIC,
                &test_push(),
                TIMEOUT
            ),
            Err(SendError::Other(ROOT_SERVER_ONLY.into()))
        );
    }
    assert_eq!(server.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

#[test]
fn send_reports_forbidden_and_does_not_follow_redirects() {
    let (server, worker) = stub(|mut stream| {
        read_publish(&mut stream);
        reply(&mut stream, "403 Forbidden");
    });
    assert_eq!(
        send(&server, TOPIC, &test_push(), TIMEOUT),
        Err(SendError::Status(403))
    );
    worker.join().unwrap();

    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    sink.set_nonblocking(true).unwrap();
    let destination = sink.local_addr().unwrap();
    let (server, worker) = stub(move |mut stream| {
        read_publish(&mut stream);
        write!(
            stream,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{destination}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ).unwrap();
    });
    assert_eq!(
        send(&server, TOPIC, &test_push(), TIMEOUT),
        Err(SendError::Status(307))
    );
    worker.join().unwrap();
    assert_eq!(sink.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

#[test]
fn send_reports_a_refused_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(
        send(&server, TOPIC, &test_push(), TIMEOUT),
        Err(SendError::Connect)
    );
}

#[test]
fn send_times_out_when_the_server_does_not_answer() {
    let (release, wait) = mpsc::channel();
    let (server, worker) = stub(move |stream| {
        let _ = wait.recv_timeout(TIMEOUT);
        drop(stream);
    });
    let started = Instant::now();
    let result = send(&server, TOPIC, &test_push(), Duration::from_millis(150));
    let elapsed = started.elapsed();
    let _ = release.send(());
    assert_eq!(result, Err(SendError::Timeout));
    assert!(elapsed < TIMEOUT, "elapsed: {elapsed:?}");
    worker.join().unwrap();
}

fn wait_last(pusher: &Pusher, expected: Result<(), SendError>) -> LastPush {
    let started = Instant::now();
    loop {
        if let Some(last) = pusher.last()
            && last.result == expected
        {
            return last;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "missing auto push result {expected:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn last_tracks_auto_failure_and_success_but_ignores_test_pushes() {
    let pusher = Pusher::new();
    let (manual, worker) = stub(|mut stream| {
        read_publish(&mut stream);
        reply(&mut stream, "200 OK");
    });
    assert_eq!(send(&manual, TOPIC, &test_push(), TIMEOUT), Ok(()));
    worker.join().unwrap();
    assert!(pusher.last().is_none());

    let (failure, worker) = stub(|mut stream| {
        read_publish(&mut stream);
        reply(&mut stream, "403 Forbidden");
    });
    let mut settings = Settings {
        server: failure,
        topic: TOPIC.into(),
        ..selected()
    };
    let saved = Arc::new(NotifySettings::default());
    saved.publish(Ok(settings.clone()));
    pusher.notify(&finished("partial"), &saved);
    let failed = wait_last(&pusher, Err(SendError::Status(403)));
    worker.join().unwrap();

    let (manual, worker) = stub(|mut stream| {
        read_publish(&mut stream);
        reply(&mut stream, "200 OK");
    });
    assert_eq!(send(&manual, TOPIC, &test_push(), TIMEOUT), Ok(()));
    worker.join().unwrap();
    let last = pusher.last().unwrap();
    assert_eq!(last.at, failed.at);
    assert_eq!(last.result, failed.result);

    let (success, worker) = stub(|mut stream| {
        let body = read_publish(&mut stream);
        reply(&mut stream, "200 OK");
        body
    });
    settings.server = success;
    saved.publish(Ok(settings));
    pusher.notify(&finished("done"), &saved);
    wait_last(&pusher, Ok(()));
    assert_eq!(worker.join().unwrap()["message"], "工单 #42 已完成");
}

#[test]
fn saved_changes_drop_the_backlog_and_allow_an_in_flight_push_to_finish() {
    for enabled in [true, false] {
        let (sent_tx, sent_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (pusher, worker) = Pusher::start(move |_, topic, push, _| {
            sent_tx
                .send((topic.to_owned(), push.message.clone()))
                .unwrap();
            if push.message.starts_with("工单 #1 ") {
                release_rx.recv_timeout(TIMEOUT).unwrap();
            }
            Ok(())
        });
        let settings = Arc::new(NotifySettings::default());
        let original = Settings {
            topic: TOPIC.into(),
            ..selected()
        };
        settings.publish(Ok(original.clone()));
        for ticket in 1..=4 {
            pusher.notify(&Event::Question { ticket }, &settings);
        }
        assert_eq!(
            sent_rx.recv_timeout(TIMEOUT).unwrap(),
            (TOPIC.into(), "工单 #1 需要你回答问题".into())
        );
        let mut changed = original;
        if enabled {
            changed.topic = "new-private-topic".into();
        } else {
            changed.enabled = false;
        }
        settings.publish(Ok(changed));
        release_tx.send(()).unwrap();
        drop(pusher);
        worker.join().unwrap();
        let unexpected: Vec<_> = sent_rx.into_iter().collect();
        assert!(unexpected.is_empty(), "{unexpected:?}");
    }
}

#[test]
fn recovered_settings_do_not_replace_a_newly_saved_value() {
    let settings = NotifySettings::default();
    settings.publish(Err("fixture read error".into()));
    let saved = Settings {
        topic: TOPIC.into(),
        ..selected()
    };
    settings.publish(Ok(saved.clone()));
    settings.recover(Settings::default());
    assert_eq!(settings.get(), Ok(saved));
}
