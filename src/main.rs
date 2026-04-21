mod audio;
mod bluetooth;
mod tui;

use bluetooth::{connect_system_dbus, get_known_devices, try_get_adapter_info};

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    RunTui,
    PrintVersion,
}

fn parse_cli<S: AsRef<str>>(args: &[S]) -> CliAction {
    match args.get(1).map(|s| s.as_ref()) {
        Some("version") | Some("--version") | Some("-V") => CliAction::PrintVersion,
        _ => CliAction::RunTui,
    }
}

fn version_string() -> String {
    format!("funke {}", env!("CARGO_PKG_VERSION"))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if parse_cli(&args) == CliAction::PrintVersion {
        println!("{}", version_string());
        return;
    }

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

    let adapter = match try_get_adapter_info(&connection).await {
        Ok(info) => info, // Some(adapter) or None
        Err(e) => {
            eprintln!("Error: Could not query Bluetooth adapter: {e}");
            eprintln!("Make sure BlueZ is installed and the bluetooth service is running.");
            std::process::exit(1);
        }
    };

    let devices = if adapter.is_some() {
        match get_known_devices(&connection).await {
            Ok(devs) => devs,
            Err(e) => {
                eprintln!("Failed to fetch known devices: {e}");
                std::process::exit(1);
            }
        }
    } else {
        Vec::new()
    };

    // Register the BlueZ pairing agent (may fail if adapter is absent)
    let (agent_tx, agent_rx) = tokio::sync::mpsc::unbounded_channel();
    let agent_registered = if adapter.is_some() {
        if let Err(e) = bluetooth::register_agent(&connection, agent_tx.clone()).await {
            eprintln!("Failed to register pairing agent: {e}");
            std::process::exit(1);
        }
        true
    } else {
        false
    };

    let result = tui::run(adapter, devices, connection.clone(), agent_rx, agent_tx, agent_registered).await;

    // Unregister the pairing agent on exit
    let _ = bluetooth::unregister_agent(&connection).await;

    if let Err(e) = result {
        eprintln!("TUI error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cli_no_args() {
        let args = vec!["funke"];
        assert_eq!(parse_cli(&args), CliAction::RunTui);
    }

    #[test]
    fn test_parse_cli_version_subcommand() {
        let args = vec!["funke", "version"];
        assert_eq!(parse_cli(&args), CliAction::PrintVersion);
    }

    #[test]
    fn test_parse_cli_double_dash_version() {
        let args = vec!["funke", "--version"];
        assert_eq!(parse_cli(&args), CliAction::PrintVersion);
    }

    #[test]
    fn test_parse_cli_short_v() {
        let args = vec!["funke", "-V"];
        assert_eq!(parse_cli(&args), CliAction::PrintVersion);
    }

    #[test]
    fn test_parse_cli_unknown_arg_falls_through() {
        let args = vec!["funke", "bogus"];
        assert_eq!(parse_cli(&args), CliAction::RunTui);
    }

    #[test]
    fn test_version_string_format() {
        let s = version_string();
        assert!(s.starts_with("funke "), "unexpected prefix in {s}");
        assert!(
            s.contains(env!("CARGO_PKG_VERSION")),
            "missing CARGO_PKG_VERSION in {s}"
        );
    }
}
