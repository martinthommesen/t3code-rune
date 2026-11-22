#![forbid(unsafe_code)]

mod client_settings;
mod composer_context;
mod connection;
mod server;
mod snooze;
mod ui;

use clap::{Parser, Subcommand};
use std::{env, error::Error, ffi::OsString, io, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "T3 Code's native Rune client")]
struct Arguments {
    #[arg(long, value_name = "URL")]
    url: Option<String>,

    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "url",
        help = "Open a database with the built-in local server"
    )]
    local_database: Option<PathBuf>,

    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["url", "local_database"],
        help = "Open the built-in server in a T3 Code home directory"
    )]
    base_dir: Option<PathBuf>,

    #[arg(
        long,
        conflicts_with_all = ["url", "local_database", "base_dir"],
        help = "Open the built-in server in the standard desktop profile"
    )]
    desktop: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Read the appearance preference from an Electron desktop profile"
    )]
    electron_profile: Option<PathBuf>,

    #[arg(
        long,
        requires = "url",
        help = "Read a pairing code from standard input"
    )]
    pairing_code_stdin: bool,

    #[command(subcommand)]
    command: Option<Command>,

    #[arg(last = true, value_name = "RUNE_ARGS")]
    rune_args: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Browse a T3 Code database through a local read-only server.
    Serve {
        #[arg(long, value_name = "PATH")]
        database: PathBuf,

        #[arg(long, default_value_t = 0)]
        port: u16,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse();
    if let Some(Command::Serve { database, port }) = arguments.command {
        return server::run(database, port);
    }

    let database = arguments.local_database.or_else(|| {
        if arguments.url.is_some() {
            return None;
        }
        let base_dir = arguments
            .base_dir
            .or_else(|| configured_home(env::var_os("T3CODE_HOME")))
            .or_else(|| {
                if arguments.desktop {
                    env::home_dir().map(|home| home.join(".t3"))
                } else {
                    None
                }
            })?;
        Some(base_dir.join("userdata").join("statev2.sqlite"))
    });
    if let Some(database) = database {
        let host = server::LocalHost::start(database)?;
        ui::run(
            Some(host.url()),
            Some(host.pairing_code()),
            arguments.rune_args,
            Some(host.state_dir()),
            arguments.electron_profile.as_deref(),
        )
        .map_err(io::Error::other)?;
        return Ok(());
    }
    if arguments.desktop {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "The T3 Code desktop profile could not be located.",
        )
        .into());
    }

    let pairing_code = if arguments.pairing_code_stdin {
        let mut code = String::new();
        io::stdin().read_line(&mut code)?;
        let code = code.trim();
        if code.is_empty() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "Pairing code is empty").into(),
            );
        }
        Some(code.to_owned())
    } else {
        None
    };

    ui::run(
        arguments.url,
        pairing_code,
        arguments.rune_args,
        None,
        arguments.electron_profile.as_deref(),
    )
    .map_err(io::Error::other)?;
    Ok(())
}

fn configured_home(value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if let Some(text) = value.to_str() {
        let text = text.trim();
        return (!text.is_empty()).then(|| PathBuf::from(text));
    }
    Some(PathBuf::from(value))
}
