mod common;
use common::*;

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires SA_LIVE_SCM_URL and authenticated gh; reads a PR without modifying it"]
async fn authenticated_pr_refresh_reaches_the_next_authorized_turn() {
    let url=std::env::var("SA_LIVE_SCM_URL").expect("SA_LIVE_SCM_URL");
    let fx=Fx::new(script(move |c|async move{
        assert_eq!(c.packet["scm_feedback"].as_array().unwrap().len(),1);
        assert!(c.packet["scm_feedback"][0]["summary"]["head"].as_str().is_some_and(|s|s.len()>=40));
        Ok("read-only PR evidence received".into())
    }),t1_catalog());
    let ticket=fx.create(fx.params()).await;
    let job=fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.bind_scm(job,&url).unwrap();
    assert!(fx.engine.refresh_scm(job).await.unwrap());
    assert_eq!(fx.engine.root(ticket).unwrap()["state"],"queued");
    fx.engine.start().unwrap();
    fx.finish(ticket,"done").await;
    assert!(fx.engine.db().with(|q|sa_engine::scm::context(q,job)).unwrap().is_empty());
}
