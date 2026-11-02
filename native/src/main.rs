//! Line-framed private stdio service. Only the local Node broker starts it.
mod context;
mod owner;
mod project;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

#[derive(Deserialize)]
struct Request {
    id: u64,
    method: String,
    params: Value,
}

fn main() -> Result<()> {
    let mut owner = None;
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in io::stdin().lock().lines() {
        let req: Request = serde_json::from_str(&line?)?;
        let p = req.params;
        let result: Result<Value> = (|| match req.method.as_str() {
            "claim" => {
                if owner.is_some() {
                    bail!("工作台已经持有执行锁");
                }
                owner = Some(owner::claim(serde_json::from_value(p)?)?);
                Ok(json!(true))
            }
            "files" => project::files(serde_json::from_value(p)?),
            "snapshot" => project::snapshot(serde_json::from_value(p)?),
            "compact" => context::compact(serde_json::from_value(p)?),
            _ => anyhow::bail!("未知原生操作"),
        })();
        let response = match result {
            Ok(value) => json!({"id":req.id,"value":value}),
            Err(error) => json!({"id":req.id,"error":format!("{error:#}")}),
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
