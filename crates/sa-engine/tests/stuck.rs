mod common;
use common::*;
use sa_engine::adapter::{AdapterError, AdapterEvent, ErrorClass};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn semantic_loop_stops_continuous_job_without_auto_retry_and_survives_restart() {
    let mut fx = Fx::new(
        script(|c| async move {
            for i in 0..12 {
                for event in [
                    json!({"type":"tool_call","callId":i.to_string(),"tool":"pwsh","input":{"command":"broken-build"}}),
                    json!({"type":"tool_result","callId":i.to_string(),"status":"error","result":{"error":"unchanged"}}),
                ] {
                    let _ = c.events.send(AdapterEvent::Journal {
                        kind: "native".into(),
                        data: event,
                    });
                }
            }
            c.cancel.cancelled().await;
            Err(AdapterError::new(
                ErrorClass::Cancelled,
                "cancelled fixture",
            ))
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().lead("deepseek").profile("auto")).await;
    fx.finish(ticket, "failed").await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='turn.loop_warning'"),
        1
    );
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='turn.loop_stopped'"),
        1
    );
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind IN ('auto.retry','turn.retry_scheduled')"),
        0
    );
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.engine.step();
    assert_eq!(fx.engine.root(ticket).unwrap()["state"], "failed");
}
