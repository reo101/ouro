use clap::{Args, Parser, Subcommand};
use port_manager::Ports;
use service::{Credentials, Service, ServiceData, Slskd, Transmission};
use std::net::Ipv4Addr;
use tokio::sync::broadcast;
use tracing::info;

pub mod firewall;
pub mod port_manager;
pub mod service;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[clap(flatten)]
    pub global_args: GlobalArgs,

    /// Service to manage (slskd or transmission)
    #[command(subcommand)]
    pub service: ServiceType,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// VPN Gateway IP (typically 10.2.0.1 for many VPNs)
    #[arg(long, default_value = "10.2.0.1")]
    pub gateway: Ipv4Addr,

    /// VPN interface name
    #[arg(long, default_value = "proton0")]
    pub vpn_interface: String,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServiceType {
    Slskd,
    Transmission(TransmissionArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TransmissionArgs {
    /// Transmission RPC username
    #[arg(long)]
    pub rpc_user: Option<String>,

    /// Transmission RPC password
    #[arg(long)]
    pub rpc_pass: Option<String>,

    /// Transmission address
    #[arg(long)]
    pub address: Option<String>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();

    let mut service: ServiceData = match args.service {
        ServiceType::Slskd => ServiceData::Slskd(Slskd {
            current_ports: Ports { udp: 0, tcp: 0 },
        }),
        ServiceType::Transmission(TransmissionArgs {
            rpc_user,
            rpc_pass,
            address,
        }) => {
            let rpc_credentials = match (rpc_user, rpc_pass) {
                (Some(username), Some(password)) => Some(Credentials { username, password }),
                _ => None,
            };

            if let Some(addr) = &address {
                info!("Using {} for transmission", addr);
            }

            ServiceData::Transmission(Transmission {
                rpc_credentials,
                address,
                current_ports: Ports { udp: 0, tcp: 0 },
            })
        }
    };

    // TODO: `oneshot`?
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal");
            shutdown_tx.send(()).ok();
        }
    });

    service.run(&args.global_args, shutdown_rx).await?;

    Ok(())
}
