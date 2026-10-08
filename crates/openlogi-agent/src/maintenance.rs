//! Installer handoff, invoked before MSI changes any application files.
//! Device restoration stays in the running agent; this process only uses IPC.

use std::time::{Duration, Instant};

use openlogi_core::single_instance::{self, InstanceError, Role};
use openlogi_ipc::{ClientKind, client};

/// Return a checked exit code to Windows Installer, including on silent runs.
#[expect(
    clippy::exit,
    reason = "MSI must receive the maintenance result before changing files"
)]
pub(crate) fn run() -> ! {
    let result = openlogi_core::worker::runtime()
        .map_err(|error| error.to_string())
        .and_then(|runtime| runtime.block_on(prepare()));
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            tracing::error!(%error, "installer handoff refused; quit or restore OpenLogi before retrying");
            std::process::exit(1);
        }
    }
}

async fn prepare() -> Result<(), String> {
    if agent_stopped()? {
        crate::tray_windows::close_gui_for_quit()?;
        return Ok(());
    }
    // A skewed or hung agent must be closed by its own tray, never force-killed.
    let agent = client::connect_as(ClientKind::Cli)
        .await
        .map_err(|error| error.to_string())?;
    let mut context = tarpc::context::current();
    context.deadline = Instant::now() + Duration::from_secs(100);
    agent
        .prepare_update(context)
        .await
        .map_err(|error| error.to_string())??;
    // Stop the GUI's reconnect/spawn loop before asking the agent to leave.
    crate::tray_windows::close_gui_for_quit()?;
    crate::overlay::evict_on_quit();
    // The server can exit before its RPC response reaches us. The process lock
    // below, rather than successful response delivery, proves teardown ended.
    let _ = agent.quit_for_update(tarpc::context::current()).await;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !agent_stopped()? {
        if Instant::now() >= deadline {
            return Err("Agent did not finish restoring device state; installation aborted".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

fn agent_stopped() -> Result<bool, String> {
    match single_instance::acquire(Role::Agent) {
        Ok(_guard) => Ok(true),
        Err(InstanceError::AlreadyRunning { .. }) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}
