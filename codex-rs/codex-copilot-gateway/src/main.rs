use clap::Parser;
use clap::Subcommand;

#[derive(Debug, Parser)]
#[command(name = "codex-copilot-gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start {
        #[arg(long, default_value_t = 4141)]
        port: u16,
    },
    Login {
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("ERROR: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start { port: 4141 }) {
        Command::Start { port } => codex_copilot_gateway::run_server(port).await,
        Command::Login { force } => {
            if let Some(login) = codex_copilot_gateway::login(force).await? {
                eprintln!("Logged in as {login}");
            }
            Ok(())
        }
    }
}
