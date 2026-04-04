use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::devices::DevicesCommand;

/// A paired device entry.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct PairedDevice {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    paired_at: Option<String>,
}

/// Load paired devices from ~/.rsclaw/devices/paired.json.
fn paired_json_path() -> std::path::PathBuf {
    let home = dirs_next::home_dir().unwrap_or_default();

    // Prefer RSCLAW_BASE_DIR if set.
    let base = std::env::var("RSCLAW_BASE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home.join(".rsclaw"));

    base.join("devices").join("paired.json")
}

fn load_devices() -> Result<Vec<PairedDevice>> {
    let path = paired_json_path();
    if path.exists() {
        let data = std::fs::read_to_string(&path)?;
        let devices: Vec<PairedDevice> = serde_json::from_str(&data)?;
        return Ok(devices);
    }
    Ok(Vec::new())
}

fn save_devices(devices: &[PairedDevice]) -> Result<()> {
    let path = paired_json_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(devices)?)?;
    Ok(())
}

pub async fn cmd_devices(sub: DevicesCommand) -> Result<()> {
    match sub {
        DevicesCommand::List => {
            let devices = load_devices()?;
            if devices.is_empty() {
                println!("no paired devices");
            } else {
                for d in &devices {
                    let name = d.name.as_deref().unwrap_or("unnamed");
                    let role = d.role.as_deref().unwrap_or("unknown");
                    let status = d.status.as_deref().unwrap_or("unknown");
                    let paired = d.paired_at.as_deref().unwrap_or("-");
                    println!(
                        "{}  name:{name}  role:{role}  status:{status}  paired:{paired}",
                        d.id
                    );
                }
            }
        }
        DevicesCommand::Approve { id } => {
            let mut devices = load_devices()?;
            if let Some(d) = devices.iter_mut().find(|d| d.id == id) {
                d.status = Some("approved".to_owned());
                save_devices(&devices)?;
                println!("approved device '{id}'");
            } else {
                anyhow::bail!("device '{id}' not found");
            }
        }
        DevicesCommand::Reject { id } => {
            let mut devices = load_devices()?;
            if let Some(d) = devices.iter_mut().find(|d| d.id == id) {
                d.status = Some("rejected".to_owned());
                save_devices(&devices)?;
                println!("rejected device '{id}'");
            } else {
                anyhow::bail!("device '{id}' not found");
            }
        }
        DevicesCommand::Remove { id } => {
            let mut devices = load_devices()?;
            let before = devices.len();
            devices.retain(|d| d.id != id);
            if devices.len() == before {
                anyhow::bail!("device '{id}' not found");
            }
            save_devices(&devices)?;
            println!("removed device '{id}'");
        }
        DevicesCommand::Revoke { role } => {
            let mut devices = load_devices()?;
            let mut count = 0usize;
            for d in &mut devices {
                if d.role.as_deref() == Some(&role) {
                    d.status = Some("revoked".to_owned());
                    count += 1;
                }
            }
            if count == 0 {
                anyhow::bail!("no devices with role '{role}' found");
            }
            save_devices(&devices)?;
            println!("revoked {count} device(s) with role '{role}'");
        }
        DevicesCommand::Rotate { role } => {
            let mut devices = load_devices()?;
            let mut count = 0usize;
            for d in &mut devices {
                if d.role.as_deref() == Some(&role) {
                    d.status = Some("token-rotated".to_owned());
                    count += 1;
                }
            }
            if count == 0 {
                anyhow::bail!("no devices with role '{role}' found");
            }
            save_devices(&devices)?;
            println!("rotated tokens for {count} device(s) with role '{role}'");
        }
        DevicesCommand::Clear => {
            save_devices(&[])?;
            println!("cleared all paired devices");
        }
    }
    Ok(())
}
