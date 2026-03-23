mod audio;
mod bluetooth;
mod tui;

use bluetooth::{connect_system_dbus, get_adapter_info, get_known_devices};

#[tokio::main]
async fn main() {
    // Install a panic hook that restores the terminal before printing the panic.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        default_hook(info);
    }));

    let connection = match connect_system_dbus().await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Error: Could not connect to system D-Bus: {e}");
            eprintln!("Make sure D-Bus is running and you have permissions to access the system bus.");
            std::process::exit(1);
        }
    };

    let adapter = match get_adapter_info(&connection).await {
        Ok(info) => info,
        Err(e) => {
            eprintln!("Error: Could not find a Bluetooth adapter: {e}");
            eprintln!("Make sure BlueZ is installed and the bluetooth service is running.");
            std::process::exit(1);
        }
    };

    let devices = match get_known_devices(&connection).await {
        Ok(devs) => devs,
        Err(e) => {
            eprintln!("Failed to fetch known devices: {e}");
            std::process::exit(1);
        }
    };

    // Register the BlueZ pairing agent
    let (agent_tx, agent_rx) = tokio::sync::mpsc::unbounded_channel();
    if let Err(e) = bluetooth::register_agent(&connection, agent_tx).await {
        eprintln!("Failed to register pairing agent: {e}");
        std::process::exit(1);
    }

    let result = tui::run(adapter, devices, connection.clone(), agent_rx).await;

    // Unregister the pairing agent on exit
    let _ = bluetooth::unregister_agent(&connection).await;

    if let Err(e) = result {
        eprintln!("TUI error: {e}");
        std::process::exit(1);
    }
}
