//! Real App clicks for the phone-push dialog, with a local stub as the ntfy server.

use crate::{
    app::App,
    cli::Options,
    host::{Action, Host, tests::fakes},
    push::{self, Event, SendError},
    worker,
};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use sa_store::{TicketSummary, notify::Settings};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn harness(data: &Path, observe: bool, demo: bool) -> Harness<'static, App> {
    let dir = data.to_path_buf();
    Harness::builder()
        .with_size(egui::vec2(1280.0, 1400.0))
        .build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let host = if observe || demo {
                Err("只读预览".into())
            } else {
                Host::start_with(dir.clone(), fakes, move || ctx.request_repaint())
                    .map_err(|e| format!("{e:#}"))
            };
            let worker = worker::spawn(
                crate::data::StoreSource::opener(dir.clone()),
                cc.egui_ctx.clone(),
            );
            App::new(
                cc,
                Options {
                    data: Some(dir),
                    observe,
                    demo,
                    ..Default::default()
                },
                worker,
                Arc::new(Mutex::new(None)),
                host,
            )
        })
}

fn open(h: &mut Harness<'_, App>) {
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "手机推送与隐私").click();
    h.run_steps(3);
    assert!(h.state().notify.is_some());
    settle(h);
}

/// Modal centering follows the measured content over several frames; click
/// only once the visible targets stop moving.
fn settle(h: &mut Harness<'_, App>) {
    let mut previous = Vec::new();
    let mut stable = 0;
    for _ in 0..16 {
        let rects = [
            (Role::CheckBox, "向手机推送提醒"),
            (Role::TextInput, "服务器"),
            (Role::TextInput, "主题"),
            (Role::Button, "发送测试推送"),
            (Role::Button, "保存"),
        ]
        .map(|(role, label)| h.get_by_role_and_label(role, label).rect())
        .to_vec();
        if rects == previous {
            stable += 1;
            if stable == 2 {
                return;
            }
        } else {
            stable = 0;
        }
        previous = rects;
        h.run_steps(1);
    }
    panic!("notify dialog did not settle");
}

fn wait(h: &mut Harness<'_, App>, what: &str, cond: impl Fn(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        h.step();
        if cond(h.state()) {
            h.run_steps(3);
            if h.state().notify.is_some() {
                settle(h);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn draft(h: &Harness<'_, App>) -> Settings {
    h.state().notify.as_ref().unwrap().draft.clone()
}

fn edit(h: &mut Harness<'_, App>, f: impl FnOnce(&mut Settings)) {
    f(&mut h.state_mut().notify.as_mut().unwrap().draft);
    h.run_steps(3);
    settle(h);
}

fn shown(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_by_label(text).is_some()
}

fn disabled(h: &Harness<'_, App>, role: Role, label: &str) -> bool {
    h.get_by_role_and_label(role, label).accesskit_node().is_disabled()
}

const TOPIC: &str = "sa-AbCdEfGhIjKlMnOpQrSt";

#[test]
fn master_switch_off_disables_the_form_and_hides_validation() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    assert_eq!(draft(&h), Settings::default());
    assert!(!shown(&h, "请填写主题"));
    assert!(!shown(&h, "未选择任何提醒，将不会收到推送。"));
    for label in ["服务器", "主题"] {
        assert!(disabled(&h, Role::TextInput, label), "{label}");
    }
    assert!(disabled(&h, Role::Button, "生成随机主题"));
    assert!(disabled(&h, Role::Button, "发送测试推送"));
    assert!(disabled(&h, Role::CheckBox, "已完成"));
    assert!(!disabled(&h, Role::CheckBox, "向手机推送提醒"));
    // Invalid values are not reported while the switch is off.
    edit(&mut h, |s| {
        s.topic = "bad topic".into();
        s.server = "ftp://x".into();
    });
    assert!(!shown(&h, "主题只能包含字母、数字、- 和 _，最多 64 个字符"));
    assert!(!shown(&h, "请使用 https 地址；只有本机或局域网地址可以用 http"));
    assert!(!disabled(&h, Role::Button, "保存"));
}

#[test]
fn validation_texts_follow_the_spec() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    h.get_by_role_and_label(Role::CheckBox, "向手机推送提醒").click();
    h.run_steps(5);
    assert!(draft(&h).enabled);
    assert!(!disabled(&h, Role::TextInput, "服务器"));
    assert!(shown(&h, "请填写主题"));
    assert!(disabled(&h, Role::Button, "保存"));
    assert!(disabled(&h, Role::Button, "发送测试推送"));

    edit(&mut h, |s| s.topic = "bad topic!".into());
    assert!(shown(&h, "主题只能包含字母、数字、- 和 _，最多 64 个字符"));
    assert!(!shown(&h, "请填写主题"));

    let warning = "主题较短，容易被猜到。建议点击“生成随机主题”。";
    edit(&mut h, |s| s.topic = "abc".into());
    assert!(shown(&h, warning));
    // A short topic only warns; saving stays possible.
    assert!(!disabled(&h, Role::Button, "保存"));
    assert!(!disabled(&h, Role::Button, "发送测试推送"));
    edit(&mut h, |s| s.topic = TOPIC.into());
    assert!(!shown(&h, warning));

    edit(&mut h, |s| s.server = "http://example.com".into());
    assert!(shown(&h, "请使用 https 地址；只有本机或局域网地址可以用 http"));
    assert!(disabled(&h, Role::Button, "保存"));
    edit(&mut h, |s| s.server = "http://192.168.1.5".into());
    assert!(!shown(&h, "请使用 https 地址；只有本机或局域网地址可以用 http"));
    assert!(!disabled(&h, Role::Button, "保存"));
    edit(&mut h, |s| s.server = "  ".into());
    assert!(shown(&h, "请填写服务器地址"));
    edit(&mut h, |s| s.server = "not a url".into());
    assert!(shown(&h, "服务器地址格式不对"));
}

#[test]
fn random_topic_button_fills_a_valid_topic() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    edit(&mut h, |s| s.enabled = true);
    h.get_by_role_and_label(Role::Button, "生成随机主题").click();
    h.run_steps(5);
    let topic = draft(&h).topic;
    assert_eq!(topic.len(), 23, "{topic}");
    assert!(topic.starts_with("sa-"));
    assert!(topic[3..].bytes().all(|b| b.is_ascii_alphanumeric()), "{topic}");
    assert!(!shown(&h, "请填写主题"));
    assert!(!shown(&h, "主题较短，容易被猜到。建议点击“生成随机主题”。"));
    h.get_by_role_and_label(Role::Button, "复制").click();
    h.run_steps(3);
    assert!(h.query_by_role_and_label(Role::Button, "已复制").is_some());
}

#[test]
fn topic_can_be_typed() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    edit(&mut h, |s| s.enabled = true);
    h.get_by_role_and_label(Role::TextInput, "主题").click();
    h.get_by_role_and_label(Role::TextInput, "主题").type_text("sa-typed");
    h.run_steps(5);
    assert_eq!(draft(&h).topic, "sa-typed");
}

#[test]
fn deselecting_every_reminder_warns() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    edit(&mut h, |s| {
        s.enabled = true;
        s.topic = TOPIC.into();
    });
    let warning = "未选择任何提醒，将不会收到推送。";
    assert!(!shown(&h, warning));
    for label in [
        "需要你处理：有问题待回答，或有操作待审批",
        "已完成",
        "异常结束：失败、部分完成或已中断（手动停止的工单不推送）",
    ] {
        h.get_by_role_and_label(Role::CheckBox, label).click();
        h.run_steps(3);
    }
    let s = draft(&h);
    assert!(!s.needs_you && !s.done && !s.unfinished);
    assert!(shown(&h, warning));
    // The warning does not block saving.
    assert!(!disabled(&h, Role::Button, "保存"));
}

#[test]
fn save_persists_and_a_new_window_reads_it_back() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    open(&mut h);
    edit(&mut h, |s| {
        s.enabled = true;
        s.server = "http://192.168.1.5:8080".into();
        s.topic = TOPIC.into();
        s.done = false;
    });
    let want = draft(&h);
    h.get_by_role_and_label(Role::Button, "保存").click();
    wait(&mut h, "save", |app| app.notify.is_none());
    assert_eq!(h.state().host.as_ref().unwrap().notify_settings(), Ok(want.clone()));
    assert_eq!(
        h.state().composer.message.as_ref().map(|m| m.0.as_str()),
        Some("手机推送与隐私设置已保存，立即生效。")
    );
    drop(h);

    let mut h = harness(data.path(), false, false);
    open(&mut h);
    assert_eq!(draft(&h), want);
    assert!(shown(&h, "手机推送：已开启，发往 http://192.168.1.5:8080"));
    // 恢复默认 only edits the form.
    h.get_by_role_and_label(Role::Button, "恢复默认").click();
    h.run_steps(3);
    assert_eq!(draft(&h), Settings::default());
    assert_eq!(h.state().host.as_ref().unwrap().notify_settings(), Ok(want));
    h.get_by_role_and_label(Role::Button, "取消").click();
    h.run_steps(3);
    assert!(h.state().notify.is_none());
}

#[test]
fn failed_save_keeps_the_draft() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    h.state().host.as_ref().unwrap().engine().db().with(|q| {
        q.run("CREATE TRIGGER reject_notify BEFORE INSERT ON studio_notify BEGIN SELECT RAISE(ABORT,'fixture write failure'); END", &[])
    }).unwrap();
    open(&mut h);
    edit(&mut h, |s| {
        s.enabled = true;
        s.topic = TOPIC.into();
    });
    h.get_by_role_and_label(Role::Button, "保存").click();
    wait(&mut h, "failure", |app| {
        app.notify.as_ref().is_some_and(|d| !d.saving && d.error.is_some())
    });
    assert!(draft(&h).enabled);
    assert!(h.state().notify.as_ref().unwrap().error.as_ref().unwrap().starts_with("无法保存设置。"));
    assert_eq!(h.state().host.as_ref().unwrap().notify_settings(), Ok(Settings::default()));
}

#[test]
fn observer_is_read_only_but_can_copy() {
    let data = tempfile::tempdir().unwrap();
    let saved = Settings {
        enabled: true,
        server: "https://ntfy.example.org".into(),
        topic: TOPIC.into(),
        ..Default::default()
    };
    let owner = Host::start_with(data.path().into(), fakes, || {}).unwrap();
    owner
        .engine()
        .db()
        .with(|q| sa_store::notify::save(q.conn(), &saved))
        .unwrap();
    let mut h = harness(data.path(), true, false);
    open(&mut h);
    wait(&mut h, "saved values", |app| {
        app.notify.as_ref().is_some_and(|d| d.draft == saved)
    });
    assert!(shown(&h, "只读观察窗口。请在正在调度的主窗口保存设置。"));
    assert!(shown(&h, "手机推送：已开启，发往 https://ntfy.example.org"));
    for label in ["生成随机主题", "发送测试推送", "恢复默认", "保存"] {
        assert!(disabled(&h, Role::Button, label), "{label}");
    }
    for label in ["服务器", "主题"] {
        assert!(disabled(&h, Role::TextInput, label), "{label}");
    }
    assert!(disabled(&h, Role::CheckBox, "向手机推送提醒"));
    assert!(!disabled(&h, Role::Button, "复制"));
    assert!(!disabled(&h, Role::Button, "复制路径"));
    h.get_by_role_and_label(Role::Button, "复制路径").click();
    h.run_steps(3);
    assert!(h.query_by_role_and_label(Role::Button, "已复制").is_some());
    h.get_by_role_and_label(Role::Button, "取消").click();
    h.run_steps(3);
    assert!(h.state().notify.is_none());
    assert_eq!(owner.engine().db().with(|q| sa_store::notify::load(q.conn())).unwrap(), saved);
}

#[test]
fn demo_previews_controls_but_never_sends() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, true);
    open(&mut h);
    assert!(shown(&h, "演示预览，可试选控件。设置仅用于本次预览。"));
    h.get_by_role_and_label(Role::CheckBox, "向手机推送提醒").click();
    h.run_steps(3);
    edit(&mut h, |s| s.topic = TOPIC.into());
    assert!(draft(&h).enabled);
    assert!(disabled(&h, Role::Button, "发送测试推送"));
    assert!(shown(&h, "演示预览不会发送真实推送。"));
    assert!(disabled(&h, Role::Button, "保存"));
    assert!(shown(&h, "手机推送：未开启"));
}

/// One-shot ntfy stand-in: answers a single request with `status` and
/// records the request body.
fn stub(status: u16) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = Arc::new(Mutex::new(String::new()));
    let seen = body.clone();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let (head_end, length) = loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            buf.extend_from_slice(&chunk[..n]);
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..at]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (at + 4, length);
            }
            if n == 0 {
                return;
            }
        };
        while buf.len() < head_end + length {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        *seen.lock().unwrap() = String::from_utf8_lossy(&buf[head_end..]).into_owned();
        let _ = write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    });
    (url, body)
}

const SAVED_TEXT: &str = "测试推送已发送。如果手机没有收到，请检查订阅的主题是否完全一致，以及 ntfy 是否允许通知。";
const UNSAVED_TEXT: &str = "测试推送已发送。当前设置还没有保存，确认无误后请点击“保存”。如果手机没有收到，请检查订阅的主题是否完全一致，以及 ntfy 是否允许通知。";

fn send_test(h: &mut Harness<'_, App>) {
    h.get_by_role_and_label(Role::Button, "发送测试推送").click();
    h.run_steps(2);
    wait(h, "test push", |app| app.notify.as_ref().unwrap().test.is_some());
}

#[test]
fn test_push_reports_success_for_unsaved_and_saved_settings() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    let (url, body) = stub(200);
    open(&mut h);
    edit(&mut h, |s| {
        s.enabled = true;
        s.server = url.clone();
        s.topic = TOPIC.into();
    });
    send_test(&mut h);
    assert!(shown(&h, UNSAVED_TEXT));
    assert!(!shown(&h, SAVED_TEXT));
    let sent: serde_json::Value = serde_json::from_str(&body.lock().unwrap()).unwrap();
    assert_eq!(sent["topic"], TOPIC);
    assert_eq!(sent["title"], "Super Agent 测试推送");
    assert_eq!(sent["message"], "收到这条，说明推送设置正确。");

    // Editing the server or the topic clears the result.
    edit(&mut h, |s| s.topic.push('x'));
    assert!(h.state().notify.as_ref().unwrap().test.is_none());
    assert!(!shown(&h, UNSAVED_TEXT));

    // Save, reopen, and the same test reads as saved.
    h.get_by_role_and_label(Role::Button, "保存").click();
    wait(&mut h, "save", |app| app.notify.is_none());
    let (url, _) = stub(200);
    let settings = Settings { enabled: true, server: url, topic: TOPIC.into(), ..Default::default() };
    h.state_mut().submit(Action::SaveNotify(settings.clone()));
    wait(&mut h, "second save", |app| {
        app.host.as_ref().unwrap().notify_settings() == Ok(settings.clone())
    });
    open(&mut h);
    send_test(&mut h);
    assert!(shown(&h, SAVED_TEXT));
    assert!(!shown(&h, UNSAVED_TEXT));
    edit(&mut h, |s| s.server.push('/'));
    assert!(h.state().notify.as_ref().unwrap().test.is_none());
}

#[test]
fn test_push_reports_the_server_status() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    let (url, _) = stub(403);
    open(&mut h);
    edit(&mut h, |s| {
        s.enabled = true;
        s.server = url.clone();
        s.topic = TOPIC.into();
    });
    send_test(&mut h);
    assert!(shown(&h, "发送失败：服务器返回 403"));
    edit(&mut h, |s| s.topic = "sa-another-topic-1".into());
    assert!(!shown(&h, "发送失败：服务器返回 403"));
}

#[test]
fn test_push_to_a_closed_port_reports_a_connection_failure() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    open(&mut h);
    edit(&mut h, |s| {
        s.enabled = true;
        s.server = format!("http://127.0.0.1:{port}");
        s.topic = TOPIC.into();
    });
    send_test(&mut h);
    assert!(shown(&h, "发送失败：无法连接服务器"));
}

fn row(id: i64, state: &str) -> TicketSummary {
    TicketSummary {
        id,
        goal: "g".into(),
        cwd: String::new(),
        lead: "claude".into(),
        created: 0.0,
        state: state.into(),
        title: None,
        pinned: false,
        archived: false,
        attention: None,
    }
}

fn save_settings(h: &mut Harness<'_, App>, settings: Settings) {
    h.state_mut().submit(Action::SaveNotify(settings.clone()));
    wait(h, "settings", |app| app.host.as_ref().unwrap().notify_settings() == Ok(settings.clone()));
}

fn enabled() -> Settings {
    Settings { enabled: true, topic: TOPIC.into(), ..Default::default() }
}

#[test]
fn finished_tickets_push_only_while_unfocused_and_cancelled_never() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    h.run_steps(3);
    save_settings(&mut h, enabled());
    let app = h.state_mut();
    app.test_focus = Some(false);
    // First sight of a ticket is a baseline, never a push.
    app.notice_transitions(&[row(901, "running"), row(902, "running"), row(903, "running"), row(904, "running")]);
    assert!(app.pushed.is_empty());
    app.notice_transitions(&[row(901, "done"), row(902, "cancelled"), row(903, "failed")]);
    let messages: Vec<_> = app.pushed.iter().map(|p| p.message.as_str()).collect();
    assert_eq!(messages, ["工单 #901 已完成", "工单 #903 失败"]);

    // The same transition in a focused window stays silent.
    app.pushed.clear();
    app.test_focus = Some(true);
    app.notice_transitions(&[row(904, "done")]);
    assert!(app.pushed.is_empty());
}

#[test]
fn questions_and_approvals_follow_the_same_gate_and_selection() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    h.run_steps(3);
    save_settings(&mut h, Settings { done: false, ..enabled() });
    let app = h.state_mut();
    app.test_focus = Some(true);
    app.push_event(Event::Question { ticket: 7 });
    assert!(app.pushed.is_empty());
    app.test_focus = Some(false);
    app.push_event(Event::Question { ticket: 7 });
    app.push_event(Event::Approval { ticket: 8 });
    app.push_event(Event::Finished { ticket: 9, state: "done".into() });
    let messages: Vec<_> = app.pushed.iter().map(|p| p.message.as_str()).collect();
    assert_eq!(messages, ["工单 #7 需要你回答问题", "工单 #8 等待你审批"]);
}

#[test]
fn nothing_is_pushed_when_the_switch_is_off_or_there_is_no_scheduler() {
    let data = tempfile::tempdir().unwrap();
    let mut h = harness(data.path(), false, false);
    h.run_steps(3);
    let app = h.state_mut();
    app.test_focus = Some(false);
    app.push_event(Event::Approval { ticket: 1 });
    assert!(app.pushed.is_empty());
    drop(h);
    let mut h = harness(data.path(), true, false);
    h.run_steps(3);
    let app = h.state_mut();
    app.test_focus = Some(false);
    app.push_event(Event::Approval { ticket: 1 });
    assert!(app.pushed.is_empty());
}

#[test]
fn forward_gate_and_event_mapping() {
    let on = Ok(enabled());
    let seen = std::cell::RefCell::new(Vec::new());
    let sink = |event: Event, _: &Settings| {
        seen.borrow_mut().push(push::push_for(&event, &enabled()).map(|p| p.message));
    };
    super::notify_ui::forward(true, Event::Approval { ticket: 1 }, Some(&on), sink);
    assert!(seen.borrow().is_empty());
    let err = Err("读取手机推送设置失败".to_string());
    super::notify_ui::forward(false, Event::Approval { ticket: 1 }, Some(&err), sink);
    super::notify_ui::forward(false, Event::Approval { ticket: 1 }, None, sink);
    assert!(seen.borrow().is_empty());
    super::notify_ui::forward(false, Event::Finished { ticket: 2, state: "cancelled".into() }, Some(&on), sink);
    assert_eq!(*seen.borrow(), [None]);
    assert_eq!(
        push::push_for(&Event::Finished { ticket: 2, state: "partial".into() }, &enabled()).unwrap().message,
        "工单 #2 部分完成"
    );
    assert_eq!(push::send("x", "y", &push::test_push(), Duration::from_secs(1)), Err(SendError::Other("服务器地址格式不对".into())));
}

#[test]
fn notify_dialog_flag_is_supported() {
    let opts = crate::cli::parse(["--demo", "--dialog", "notify"].map(str::to_string)).unwrap();
    assert_eq!(opts.dialog.as_deref(), Some("notify"));
}
