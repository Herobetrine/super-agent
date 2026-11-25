//! Direct ntfy publishing and the background queue for automatic pushes.

use sa_store::notify::{self, ServerProblem, Settings, TopicProblem};
use std::io::ErrorKind;
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, SystemTime};
use ureq::Error;
use url::Url;

const AUTO_TIMEOUT: Duration = Duration::from_secs(10);

pub enum Event {
    Question { ticket: i64 },
    Approval { ticket: i64 },
    Finished { ticket: i64, state: String },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Push {
    pub title: String,
    pub message: String,
    pub priority: u8,
    pub tags: Vec<&'static str>,
}

pub fn push_for(event: &Event, s: &Settings) -> Option<Push> {
    if !s.enabled {
        return None;
    }
    let (ticket, status, priority, tag) = match event {
        Event::Question { ticket } if s.needs_you => (*ticket, "需要你回答问题", 4, "question"),
        Event::Approval { ticket } if s.needs_you => (*ticket, "等待你审批", 4, "raised_hand"),
        Event::Finished { ticket, state } => match state.as_str() {
            "done" if s.done => (*ticket, "已完成", 3, "white_check_mark"),
            "failed" if s.unfinished => (*ticket, "失败", 4, "x"),
            "partial" if s.unfinished => (*ticket, "部分完成", 4, "warning"),
            "interrupted" if s.unfinished => (*ticket, "已中断", 4, "pause_button"),
            _ => return None,
        },
        _ => return None,
    };
    Some(Push {
        title: "Super Agent".into(),
        message: format!("工单 #{ticket} {status}"),
        priority,
        tags: vec![tag],
    })
}

pub fn test_push() -> Push {
    Push {
        title: "Super Agent 测试推送".into(),
        message: "收到这条，说明推送设置正确。".into(),
        priority: 3,
        tags: vec![],
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    Connect,
    Timeout,
    Tls,
    Status(u16),
    Other(String),
}

// Use the same direct-connection and system trust policy as the adapter catalog.
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent("SuperAgent")
        .build()
        .into()
});

pub fn send(server: &str, topic: &str, push: &Push, timeout: Duration) -> Result<(), SendError> {
    let check = notify::check(&Settings {
        enabled: true,
        server: server.into(),
        topic: topic.into(),
        ..Settings::default()
    });
    let problem = match (check.server, check.topic) {
        (Some(ServerProblem::Empty), _) => Some("请填写服务器地址"),
        (Some(ServerProblem::NeedsHttps), _) => {
            Some("请使用 https 地址；只有本机或局域网地址可以用 http")
        }
        (Some(ServerProblem::Invalid), _) => Some("服务器地址格式不对"),
        (_, Some(TopicProblem::Empty)) => Some("请填写主题"),
        (_, Some(TopicProblem::Invalid)) => Some("主题只能包含字母、数字、- 和 _，最多 64 个字符"),
        (None, None) => None,
    };
    if let Some(problem) = problem {
        return Err(SendError::Other(problem.into()));
    }
    let target =
        Url::parse(server.trim()).map_err(|_| SendError::Other("服务器地址格式不对".into()))?;
    let body = serde_json::json!({
        "topic": topic,
        "title": push.title,
        "message": push.message,
        "priority": push.priority,
        "tags": push.tags,
    });
    let response = HTTP
        .post(target.as_str())
        .header("Content-Type", "application/json")
        .config()
        .timeout_global(Some(timeout))
        .build()
        .send(body.to_string())
        .map_err(|error| send_error(&error, target.scheme() == "https"))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(SendError::Status(status.as_u16()))
    }
}

fn send_error(error: &Error, tls: bool) -> SendError {
    match error {
        Error::Timeout(_) => SendError::Timeout,
        Error::Tls(_) | Error::Rustls(_) | Error::TlsRequired | Error::Pem(_) => SendError::Tls,
        Error::ConnectionFailed | Error::HostNotFound => SendError::Connect,
        Error::StatusCode(status) => SendError::Status(*status),
        Error::Io(error) => {
            // The Rustls transport can wrap an underlying ureq transport error.
            if let Some(inner) = error.get_ref().and_then(|e| e.downcast_ref::<Error>()) {
                return send_error(inner, tls);
            }
            match error.kind() {
                ErrorKind::TimedOut | ErrorKind::WouldBlock => SendError::Timeout,
                ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::NetworkDown
                | ErrorKind::AddrNotAvailable => SendError::Connect,
                // Rustls reports handshake/certificate failures via InvalidData.
                ErrorKind::InvalidData if tls => SendError::Tls,
                _ => SendError::Other("网络读写失败".into()),
            }
        }
        Error::Http(_) | Error::BadUri(_) => SendError::Other("服务器地址格式不对".into()),
        Error::Protocol(_) | Error::LargeResponseHeader(..) => {
            SendError::Other("服务器响应格式不对".into())
        }
        _ => SendError::Other("推送请求失败".into()),
    }
}

pub struct Pusher {
    queue: mpsc::Sender<(String, String, Push)>,
    last: Arc<Mutex<Option<LastPush>>>,
}

#[derive(Clone, Debug)]
pub struct LastPush {
    pub at: SystemTime,
    pub result: Result<(), SendError>,
}

impl Pusher {
    pub fn new() -> Self {
        let (queue, receiver) = mpsc::channel::<(String, String, Push)>();
        let last = Arc::new(Mutex::new(None));
        let worker_last = Arc::clone(&last);
        std::thread::spawn(move || {
            while let Ok((server, topic, push)) = receiver.recv() {
                let result = send(&server, &topic, &push, AUTO_TIMEOUT);
                *worker_last.lock().unwrap_or_else(|p| p.into_inner()) = Some(LastPush {
                    at: SystemTime::now(),
                    result,
                });
            }
        });
        Self { queue, last }
    }

    pub fn notify(&self, event: Event, settings: &Settings) {
        if let Some(push) = push_for(&event, settings) {
            let _ = self
                .queue
                .send((settings.server.clone(), settings.topic.clone(), push));
        }
    }

    pub fn last(&self) -> Option<LastPush> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests;
