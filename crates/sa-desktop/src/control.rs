//! Same-user local control spool. Only the lock-owning host serves requests.
//! Replies deliberately exclude prompts, paths, model output and credentials.
use anyhow::{Result, bail};
use sa_engine::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PROTOCOL: &str = "super-agent-local-control";
const MAX_BYTES: u64 = 4096;
const PER_TICK: usize = 8;

pub struct Server {
    root: PathBuf,
    generation: String,
    completed: HashMap<String, Value>,
}

impl Server {
    pub fn open(data: &Path) -> Result<Self> {
        let root = data.join("control");
        let generation = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        fs::create_dir_all(root.join("requests"))?;
        fs::create_dir_all(root.join("responses"))?;
        // Replace a stale marker left by an unclean shutdown after claiming the journal.
        let ready = root.join("ready.json");
        if ready.exists() {
            fs::remove_file(&ready)?;
        }
        publish(
            &ready,
            &json!({"protocol":PROTOCOL,"version":1,"pid":std::process::id(),"generation":generation}),
        )?;
        Ok(Self {
            root,
            generation,
            completed: HashMap::new(),
        })
    }

    pub async fn run(mut self, engine: Engine, wake: Arc<dyn Fn() + Send + Sync>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            // The owned engine remains the only scheduler and database writer.
            let _ = self.poll(|command, ticket| {
                match command {
                    "pause_after_handoff" => {
                        engine.request_pause_after_handoff(ticket)?;
                    }
                    "resume" => {
                        engine.resume_paused(ticket)?;
                    }
                    _ => {}
                }
                let pause = engine.pause_status(ticket)?;
                let jobs = engine.db().with(|q| {
                    q.all(
                        "SELECT id,parent,state,turn FROM jobs WHERE ticket=? ORDER BY id",
                        &[&ticket],
                    )
                })?;
                wake();
                Ok(json!({"ticket":ticket,"pause":pause,"jobs":jobs}))
            });
        }
    }

    fn poll(&mut self, mut execute: impl FnMut(&str, i64) -> Result<Value>) -> Result<()> {
        for entry in fs::read_dir(self.root.join("requests"))?
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    let path = entry.path();
                    matches!(
                        path.extension().and_then(|x| x.to_str()),
                        Some("json" | "processing")
                    ) && path
                        .file_stem()
                        .and_then(|x| x.to_str())
                        .is_some_and(valid_id)
                })
            })
            .take(PER_TICK)
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let extension = path.extension().and_then(|x| x.to_str());
            if !matches!(extension, Some("json" | "processing")) {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|x| x.to_str())
                .filter(|x| valid_id(x))
            else {
                continue;
            };
            let reply = self.root.join("responses").join(format!("{id}.json"));
            if reply.exists() {
                self.completed.remove(id);
                fs::remove_file(&path)?;
                continue;
            }
            let claimed = path.with_extension("processing");
            if path != claimed {
                // A request becomes visible only after the client's atomic rename.
                if fs::rename(&path, &claimed).is_err() {
                    continue;
                }
            }
            let response = match self.completed.get(id) {
                Some(response) => response.clone(),
                None => {
                    let result = read_request(&claimed, id, &self.generation)
                        .and_then(|(command, ticket)| execute(&command, ticket));
                    let response = match result {
                        Ok(value) => json!({"version":1,"id":id,"ok":true,"result":value}),
                        Err(_) => {
                            json!({"version":1,"id":id,"ok":false,"error":"request_rejected"})
                        }
                    };
                    // An operator action has already completed. Failed publication must
                    // retry this response without repeating a mutation after a newer UI action.
                    self.completed.insert(id.to_owned(), response.clone());
                    response
                }
            };
            publish(&reply, &response)?;
            self.completed.remove(id);
            fs::remove_file(claimed)?;
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.root.join("ready.json"));
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn read_request(path: &Path, expected_id: &str, generation: &str) -> Result<(String, i64)> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_BYTES {
        bail!("invalid request");
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("invalid request");
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid request"))?;
    let command = value["command"].as_str().unwrap_or_default();
    let ticket = value["ticket"].as_i64().unwrap_or_default();
    if object.len() != 5
        || value["generation"] != generation
        || value["version"] != 1
        || value["id"] != expected_id
        || !matches!(command, "status" | "pause_after_handoff" | "resume")
        || ticket <= 0
    {
        bail!("invalid request");
    }
    Ok((command.to_owned(), ticket))
}

fn publish(path: &Path, value: &Value) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(temporary.as_file_mut(), value)?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_validates_claims_deduplicates_and_recovers() {
        let data = tempfile::tempdir().unwrap();
        let mut server = Server::open(data.path()).unwrap();
        let requests = server.root.join("requests");
        for index in 0..16 {
            fs::write(requests.join(format!("ignored-{index}.tmp")), b"pending").unwrap();
        }
        let request = json!({"generation":server.generation,"version":1,"id":"one","command":"pause_after_handoff","ticket":7});
        publish(&requests.join("one.json"), &request).unwrap();
        let mut calls = 0;
        server
            .poll(|command, ticket| {
                calls += 1;
                assert_eq!(command, "pause_after_handoff");
                assert_eq!(ticket, 7);
                Ok(json!({"pause":"draining"}))
            })
            .unwrap();
        publish(&requests.join("one.json"), &request).unwrap();
        server
            .poll(|_, _| {
                calls += 1;
                Ok(Value::Null)
            })
            .unwrap();
        assert_eq!(calls, 1);
        let malformed =
            json!({"version":1,"id":"bad","command":"exec","ticket":7,"secret":"fixture-private"});
        publish(&requests.join("bad.processing"), &malformed).unwrap();
        server
            .poll(|_, _| panic!("rejected request executed"))
            .unwrap();
        let error = fs::read_to_string(server.root.join("responses/bad.json")).unwrap();
        assert!(error.contains("request_rejected"));
        assert!(!error.contains("fixture-private"));
        publish(&requests.join("recover.processing"), &json!({"generation":server.generation,"version":1,"id":"recover","command":"status","ticket":7})).unwrap();
        server.poll(|_, _| Ok(json!({"pause":"paused"}))).unwrap();
        assert!(server.root.join("responses/recover.json").exists());
        let ready = server.root.join("ready.json");
        drop(server);
        assert!(!ready.exists());
    }

    #[test]
    fn invalid_schema_and_oversized_requests_never_execute() {
        let data = tempfile::tempdir().unwrap();
        let mut server = Server::open(data.path()).unwrap();
        for (id, value) in [
            (
                "version",
                json!({"generation":server.generation,"version":2,"id":"version","command":"status","ticket":1}),
            ),
            (
                "identity",
                json!({"generation":server.generation,"version":1,"id":"other","command":"status","ticket":1}),
            ),
            (
                "ticket",
                json!({"generation":server.generation,"version":1,"id":"ticket","command":"resume","ticket":0}),
            ),
        ] {
            publish(&server.root.join(format!("requests/{id}.json")), &value).unwrap();
        }
        fs::write(server.root.join("requests/large.json"), vec![b'x'; 4097]).unwrap();
        server
            .poll(|_, _| panic!("invalid request executed"))
            .unwrap();
        assert_eq!(
            fs::read_dir(server.root.join("responses")).unwrap().count(),
            4
        );
    }
    #[test]
    fn failed_response_publication_does_not_repeat_a_completed_resume() {
        let data = tempfile::tempdir().unwrap();
        let mut server = Server::open(data.path()).unwrap();
        let responses = server.root.join("responses");
        let request = json!({"generation":server.generation,"version":1,"id":"resume","command":"resume","ticket":7});
        publish(&server.root.join("requests/resume.json"), &request).unwrap();
        let mut calls = 0;
        let mut paused = true;
        assert!(
            server
                .poll(|command, _| {
                    assert_eq!(command, "resume");
                    calls += 1;
                    paused = false;
                    fs::remove_dir(&responses).unwrap();
                    fs::write(&responses, b"publication unavailable").unwrap();
                    Ok(json!({"pause":"running"}))
                })
                .is_err()
        );
        assert_eq!(calls, 1);
        assert!(!paused);
        // The UI requests a new pause while the earlier response remains unpublished.
        paused = true;
        assert!(
            server
                .poll(|_, _| {
                    calls += 1;
                    paused = false;
                    Ok(Value::Null)
                })
                .is_err()
        );
        assert_eq!(calls, 1);
        assert!(paused);
        fs::remove_file(&responses).unwrap();
        fs::create_dir(&responses).unwrap();
        server
            .poll(|_, _| {
                calls += 1;
                paused = false;
                Ok(Value::Null)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert!(paused);
        let response: Value =
            serde_json::from_slice(&fs::read(responses.join("resume.json")).unwrap()).unwrap();
        assert_eq!(response["result"]["pause"], "running");
        assert!(server.completed.is_empty());
    }
}
