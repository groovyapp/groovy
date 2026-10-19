//! Send and receive files with Groovy from the command line.
//!
//! The command line uses the same data directory and identity as the desktop app,
//! so only one of them can run at a time.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use groovy_core::protocol::PeerProfile;
use groovy_core::{
    Config, ContactGate, DataDir, DataLock, Database, Identity, Network, Node, NodeConfig,
    NodeEvent, NodeHandle, Trust,
};

/// The app name the desktop app uses, so both share one data directory and identity.
const APP_NAME: &str = "groovy";

/// How long to wait for the node to come online before giving up.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the node to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(
    version,
    about = "Send and receive files with Groovy from the command line"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this device's endpoint ID and ticket.
    Id,
    /// Make a share of files or folders, print its ticket, and serve it until interrupted.
    Send {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Download a share from its ticket, and wait until its files are saved.
    Receive {
        ticket: String,
        /// The folder to save into. Defaults to the current folder.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
    /// List contacts and how far they are trusted.
    Peers,
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("groovy-cli: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to start the runtime")?;
    let setup = Setup::open()?;
    match command {
        Command::Peers => {
            for contact in setup.database.contacts()? {
                println!(
                    "{}\t{}\t{}",
                    contact.label(),
                    trust_label(contact.trust),
                    contact.endpoint_id
                );
            }
            Ok(())
        }
        Command::Id => runtime.block_on(async {
            let (node, _events) = setup.start_node(None).await?;
            let addr = node.addr().await?;
            println!("endpoint ID: {}", node.endpoint_id());
            println!("ticket: {}", groovy_core::ticket::encode(&addr));
            stop(node);
            Ok(())
        }),
        Command::Send { paths } => runtime.block_on(async {
            let (node, _events) = setup.start_node(None).await?;
            let shared = node.create_share(paths).await?;
            println!("share ticket: {}", shared.ticket);
            println!(
                "{} files, {} bytes. Anyone with the ticket can download them until you stop this command.",
                shared.share.files, shared.share.bytes
            );
            tokio::signal::ctrl_c()
                .await
                .context("failed to wait for Ctrl-C")?;
            // Stopping the share makes its files unavailable to anyone else from now on.
            node.revoke_share(shared.share.id).await?;
            println!("share stopped");
            stop(node);
            Ok(())
        }),
        Command::Receive { ticket, out } => runtime.block_on(async {
            let folder = match out {
                Some(folder) => folder,
                None => std::env::current_dir().context("failed to read the current folder")?,
            };
            let (node, mut events) = setup.start_node(Some(folder)).await?;
            let transfer = node.receive_share(ticket).await?;
            println!("receiving transfer {transfer}...");
            let outcome = loop {
                match tokio::time::timeout(Duration::from_secs(600), events.next()).await {
                    Ok(Some(NodeEvent::FileReceived { transfer: id, saved, .. }))
                        if id == transfer =>
                    {
                        break Ok(saved);
                    }
                    Ok(Some(NodeEvent::ReceiveFailed {
                        transfer: id,
                        error,
                        ..
                    })) if id == transfer => {
                        break Err(anyhow::anyhow!("the download failed: {error}"));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break Err(anyhow::anyhow!("the node stopped")),
                    Err(_) => break Err(anyhow::anyhow!("the download timed out")),
                }
            };
            stop(node);
            for path in outcome? {
                println!("saved {}", path.display());
            }
            Ok(())
        }),
    }
}

fn trust_label(trust: Trust) -> &'static str {
    match trust {
        Trust::Trusted => "trusted",
        Trust::Pending => "pending",
        Trust::Blocked => "blocked",
    }
}

/// Stops the node, and reports it if it does not stop in time.
fn stop(node: NodeHandle) {
    if !node.shutdown(STOP_TIMEOUT) {
        eprintln!("groovy-cli: the node did not stop in time");
    }
}

/// The data directory, config, database and identity, opened the way the desktop app opens them.
struct Setup {
    _lock: DataLock,
    dir: DataDir,
    config: Config,
    database: Arc<Database>,
    identity: Identity,
}

impl Setup {
    fn open() -> Result<Self> {
        let dir = DataDir::resolve(APP_NAME).context("failed to find the data directory")?;
        dir.create_all()
            .context("failed to create the data directory")?;
        let lock = match dir.lock() {
            Ok(lock) => lock,
            Err(error) => bail!("{error}"),
        };
        let config = Config::load_or_create(&dir).context("failed to load the config")?;
        let database =
            Database::open(&dir.database_file()).context("failed to open the database")?;
        let identity =
            Identity::load_or_create(&dir, APP_NAME).context("failed to load the identity")?;
        Ok(Self {
            _lock: lock,
            dir,
            config,
            database: Arc::new(database),
            identity,
        })
    }

    /// Starts the node on the public network, and waits until it is online.
    async fn start_node(
        &self,
        downloads: Option<PathBuf>,
    ) -> Result<(NodeHandle, groovy_core::EventStream)> {
        let profile = self
            .database
            .profile()?
            .map(|profile| PeerProfile::from_profile(&profile))
            .unwrap_or_default();
        let (node, mut events) = Node::start(NodeConfig {
            secret_key: self.identity.secret_key().clone(),
            network: Network::Public,
            profile,
            gate: Arc::new(ContactGate::new(self.database.clone())),
            discovery: self.config.discovery,
            relay: self.config.relay.clone(),
            blobs: Some(self.dir.blobs_dir()),
            downloads,
            database: Some(self.database.clone()),
        })
        .context("the node could not start")?;
        let started = tokio::time::timeout(START_TIMEOUT, async {
            loop {
                match events.next().await {
                    Some(NodeEvent::Started) => break Ok(()),
                    Some(_) => {}
                    None => break Err(anyhow::anyhow!("the node stopped while starting")),
                }
            }
        })
        .await
        .context("the node did not come online in time")?;
        started?;
        Ok((node, events))
    }
}
