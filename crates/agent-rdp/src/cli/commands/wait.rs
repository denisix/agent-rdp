//! Wait command implementation.

use std::time::Duration;

use tokio::time::sleep;

use crate::output::Output;

pub async fn run(ms: u64, output: &Output) -> anyhow::Result<()> {
    sleep(Duration::from_millis(ms)).await;

    // Previously produced no output at all in either mode, which meant a
    // JSON-consuming caller had to special-case `wait` as the one command
    // with an empty stdout instead of a parseable envelope.
    if output.is_json() {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "success": true,
                "data": { "type": "wait", "ms": ms }
            }))?
        );
    }

    Ok(())
}
