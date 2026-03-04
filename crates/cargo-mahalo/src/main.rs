mod commands;
mod templates;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo-mahalo", about = "Mahalo web framework CLI")]
struct Cli {
    #[command(subcommand)]
    command: CargoSubcommand,
}

#[derive(Subcommand)]
enum CargoSubcommand {
    /// Mahalo CLI commands
    Mahalo {
        #[command(subcommand)]
        command: MahaloCommand,
    },
}

#[derive(Subcommand)]
enum MahaloCommand {
    /// Create a new Mahalo project
    New {
        /// Project name
        name: String,
    },
    /// Generate code (alias: g)
    #[command(alias = "g")]
    Generate {
        #[command(subcommand)]
        kind: GenerateKind,
    },
}

#[derive(Subcommand)]
enum GenerateKind {
    /// Generate a controller
    Controller {
        /// Controller name (e.g. Room or room)
        name: String,
    },
    /// Generate a channel
    Channel {
        /// Channel name (e.g. Chat or chat)
        name: String,
    },
    /// Generate a resource (controller + routes)
    Resource {
        /// Resource name (e.g. Post or post)
        name: String,
    },
}

fn main() {
    let cli = Cli::parse();

    let CargoSubcommand::Mahalo { command } = cli.command;

    let result = match command {
        MahaloCommand::New { name } => commands::new::run(&name),
        MahaloCommand::Generate { kind } => match kind {
            GenerateKind::Controller { name } => {
                commands::generate::controller(&name)
            }
            GenerateKind::Channel { name } => {
                commands::generate::channel(&name)
            }
            GenerateKind::Resource { name } => {
                commands::generate::resource(&name)
            }
        },
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
