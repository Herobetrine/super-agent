//! Native input blocks for documented Codex app-server and Claude stream-JSON
//! protocols. Only validated snapshot bytes enter these blocks.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sa_engine::{
    adapter::{AdapterError, ErrorClass, RunRequest},
    images,
};
use serde_json::{Value, json};

pub fn check(req: &RunRequest) -> Result<(), AdapterError> {
    if req.images.is_empty() {
        return Ok(());
    }
    if !images::supports_native_images(&req.agent) {
        return Err(AdapterError::new(ErrorClass::Protocol, images::UNSUPPORTED));
    }
    if req.images.len() > images::MAX_IMAGES
        || req.images.iter().map(|i| i.data().len()).sum::<usize>() > images::MAX_TOTAL_BYTES
    {
        return Err(AdapterError::new(
            ErrorClass::InputTooLarge,
            "每轮最多 4 张图片，合计最多 16 MiB",
        ));
    }
    Ok(())
}

pub fn codex(req: &RunRequest) -> Value {
    let mut content = vec![json!({"type":"text", "text":req.prompt})];
    for image in &req.images {
        content.push(json!({"type":"image", "url":format!("data:{};base64,{}", image.media_type(), STANDARD.encode(image.data()))}));
    }
    Value::Array(content)
}

pub fn claude(req: &RunRequest) -> Value {
    if req.images.is_empty() {
        return json!(req.prompt);
    }
    let mut content = vec![json!({"type":"text", "text":req.prompt})];
    for image in &req.images {
        content.push(json!({"type":"image", "source":{
            "type":"base64", "media_type":image.media_type(), "data":STANDARD.encode(image.data())
        }}));
    }
    Value::Array(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_engine::{adapter::Route, images::ImageInput};

    #[test]
    fn native_blocks_preserve_image_bytes_for_fresh_and_resumed_turns() {
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
        let image = ImageInput::from_bytes("图.PNG".into(), png.clone()).unwrap();
        let mut req = RunRequest {
            ticket: 1,
            job: 1,
            turn: 1,
            agent: "codex".into(),
            prompt: "看图".into(),
            images: vec![image],
            cwd: ".".into(),
            route: Route {
                provider: "fixture".into(),
                model: "vision".into(),
                mode: "cli".into(), reasoning_effort: None },
            profile: "readonly".into(),
            writable: false,
            session_id: None,
        };
        for session in [None, Some("resumed-session".to_string())] {
            req.session_id = session;
            assert!(check(&req).is_ok());
            let cx = codex(&req);
            let url = cx[1]["url"].as_str().unwrap();
            assert_eq!(
                STANDARD
                    .decode(url.strip_prefix("data:image/png;base64,").unwrap())
                    .unwrap(),
                png
            );
            let cl = claude(&req);
            assert_eq!(cl[1]["source"]["media_type"], "image/png");
            assert_eq!(
                STANDARD
                    .decode(cl[1]["source"]["data"].as_str().unwrap())
                    .unwrap(),
                png
            );
        }
        req.agent = "deepseek".into();
        assert!(
            check(&req)
                .unwrap_err()
                .message
                .contains("Codex 或 Claude Code")
        );
        req.images.clear();
        assert!(check(&req).is_ok());
        assert_eq!(claude(&req), json!("看图"));
    }
}
