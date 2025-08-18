use std::{fmt::Display, path::PathBuf, str::FromStr, time::Duration};

use tokio::{fs, process::Command, sync::broadcast};
use tracing::{debug, error, info};

use crate::{
    GlobalArgs,
    firewall::manage_firewall,
    port_manager::{Ports, map_ports},
};

pub(crate) trait Service {
    const NAME: &str;

    async fn update(&self, ports: &Ports) -> eyre::Result<()>;
    async fn cleanup(&self) -> eyre::Result<()> {
        info!("Stopping slskd service...");
        Command::new("systemctl")
            .args(["stop", &format!("{}.service", Self::NAME)])
            .status()
            .await?;

        info!("Removing override configuration...");
        let override_dir = &format!("/run/systemd/system/{}.service.d", Self::NAME);
        let _ = fs::remove_dir_all(override_dir).await; // Ignore if doesn't exist

        Ok(())
    }
    // FIXME: ugly
    fn current_ports(&mut self) -> &mut Ports;

    async fn apply_ports(&mut self, interface: &str, ports: &Ports) -> eyre::Result<()> {
        let old_ports = self.current_ports();

        // Don't do anything if ports haven't changed
        if ports == old_ports {
            debug!("Ports unchanged, skipping update");
            return Ok(());
        }

        // First handle firewall
        manage_firewall(interface, Some(ports), Some(old_ports)).await?;

        // Then update service
        self.update(ports).await?;

        // Update current ports
        *self.current_ports() = ports.clone();

        Ok(())
    }

    async fn run(
        &mut self,
        args: &GlobalArgs,
        mut shutdown: broadcast::Receiver<()>,
    ) -> eyre::Result<()> {
        let client = natpmp::new_tokio_natpmp_with(args.gateway).await?;
        let mut interval = tokio::time::interval(Duration::from_secs(45));

        // Initial port mapping
        info!("Requesting initial port mapping...");
        let initial_ports = map_ports(&client).await?;
        info!(
            "Got initial ports: TCP={}, UDP={}",
            initial_ports.tcp, initial_ports.udp
        );

        self.apply_ports(&args.vpn_interface, &initial_ports)
            .await?;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Just refresh the NAT-PMP mapping without reconfiguring
                    match map_ports(&client).await {
                        Ok(new_ports) => {
                            info!(
                                current_tcp = initial_ports.tcp,
                                current_udp = initial_ports.udp,
                                new_tcp = new_ports.tcp,
                                new_udp = new_ports.udp,
                                "Checking port mapping"
                            );

                            if new_ports != initial_ports {
                                info!("Ports changed, reconfiguring service");
                                self.apply_ports(&args.vpn_interface, &new_ports).await?;
                            } else {
                                debug!("Ports unchanged, keeping current configuration");
                            }
                        }
                        Err(e) => {
                            error!("Port mapping refresh failed: {}", e);
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    }
                }
                _ = shutdown.recv() => {
                    info!("Shutting down...");
                    manage_firewall(&args.vpn_interface, None, Some(self.current_ports())).await?;
                    self.cleanup().await?;
                    break;
                }
            }
        }
        Ok(())
    }
}

pub enum ServiceData {
    Slskd(Slskd),
    Transmission(Transmission),
}

// FIXME: ugly
impl Service for ServiceData {
    const NAME: &str = "";

    async fn update(&self, ports: &Ports) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.update(ports).await,
            ServiceData::Transmission(transmission) => transmission.update(ports).await,
        }
    }

    async fn cleanup(&self) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.cleanup().await,
            ServiceData::Transmission(transmission) => transmission.cleanup().await,
        }
    }

    fn current_ports(&mut self) -> &mut Ports {
        match self {
            ServiceData::Slskd(slskd) => slskd.current_ports(),
            ServiceData::Transmission(transmission) => transmission.current_ports(),
        }
    }

    async fn apply_ports(&mut self, interface: &str, ports: &Ports) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.apply_ports(interface, ports).await,
            ServiceData::Transmission(transmission) => {
                transmission.apply_ports(interface, ports).await
            }
        }
    }

    async fn run(
        &mut self,
        args: &GlobalArgs,
        shutdown: broadcast::Receiver<()>,
    ) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.run(args, shutdown).await,
            ServiceData::Transmission(transmission) => transmission.run(args, shutdown).await,
        }
    }
}

pub struct Slskd {
    pub current_ports: Ports,
}

impl Service for Slskd {
    const NAME: &str = "slskd";

    fn current_ports(&mut self) -> &mut Ports {
        &mut self.current_ports
    }

    async fn update(&self, ports: &Ports) -> eyre::Result<()> {
        info!("Stopping slskd service...");
        Command::new("systemctl")
            .args(["stop", "slskd.service"])
            .status()
            .await?;

        let override_dir = PathBuf::from_str("/run/systemd/system/slskd.service.d")?;
        fs::create_dir_all(&override_dir).await?;

        fs::write(
            override_dir.join("override.conf"),
            format!(
                r#"
                    [Service]
                    Environment=SLSKD_SLSK_LISTEN_PORT={}
                "#,
                ports.tcp
            ),
        )
        .await?;

        Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .await?;

        info!("Starting slskd with new configuration...");
        Command::new("systemctl")
            .args(["start", "slskd.service"])
            .status()
            .await?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Transmission {
    pub rpc_credentials: Option<Credentials>,
    pub address: Option<String>,
    pub current_ports: Ports,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Display for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.username, self.password)
    }
}

impl Service for Transmission {
    const NAME: &str = "transmission";

    async fn update(&self, ports: &Ports) -> eyre::Result<()> {
        let mut cmd = Command::new("transmission-remote");
        if let Some(addr) = &self.address {
            cmd.arg(addr);
        }

        // TODO: leaked in `ps`, pass through STDIN
        self.rpc_credentials
            .as_ref()
            .map(|rpc_credentials| cmd.args(["--auth", &rpc_credentials.to_string()]));
        cmd.args(["--port", &format!("{}", ports.tcp)])
            .status()
            .await?;

        Ok(())
    }

    fn current_ports(&mut self) -> &mut Ports {
        &mut self.current_ports
    }

    async fn cleanup(&self) -> eyre::Result<()> {
        Ok(())
    }
}
