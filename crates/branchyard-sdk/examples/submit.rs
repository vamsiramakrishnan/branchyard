//! Save a command first, then: cargo run -p branchyard-sdk --example submit -- request.json
use branchyard_sdk::{Client, Command, Error, MAX_REQUEST_BYTES};
use std::io::Read;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("provide a saved command file")?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err("command too large".into());
    }
    let command: Command = serde_json::from_slice(&bytes)?;
    let client = Client::new(
        &std::env::var("BRANCHYARD_ENDPOINT")?,
        &std::env::var("BRANCHYARD_TOKEN")?,
    )?;
    match client.submit(&command).await {
        Ok(receipt) => println!("{}", serde_json::to_string(&receipt)?),
        Err(Error::SubmissionUnknown { .. }) => {
            // A lookup verifies the saved input identity. No new ID or automatic resubmission.
            println!(
                "{}",
                serde_json::to_string(&client.reconcile(&command).await?)?
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
