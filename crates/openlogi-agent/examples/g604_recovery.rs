//! Inspect or restore the G604 through the agent when the GUI is unavailable.
//! Run with OPENLOGI_PROFILE=dev for a development bundle. Hardware I/O stays
//! exclusively in the agent; this example never captures or synthesizes input.
use openlogi_core::macros::g604::G604Command;
use openlogi_ipc::{ClientKind, client};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = match std::env::args().nth(1).as_deref() {
        None | Some("status") => G604Command::Status,
        Some("restore") => G604Command::Restore,
        _ => return Err(std::io::Error::other("Expected status or restore").into()),
    };
    let agent = client::connect_as(ClientKind::Cli).await?;
    let mut context = tarpc::context::current();
    context.deadline = std::time::Instant::now() + std::time::Duration::from_secs(100);
    let status = agent
        .g604_command(context, command)
        .await?
        .map_err(std::io::Error::other)?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}
