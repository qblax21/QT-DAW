//! Finding audio hosts and devices.
//!
//! A *host* is an audio API (on Windows: WASAPI, and ASIO when built with `--features asio`).
//! A *device* is an interface or sound card that host exposes. The same hardware can show up
//! under several hosts; for recording, ASIO is the one that matters.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, SupportedStreamConfigRange};

/// Sample rates a DAW user would actually pick.
pub const COMMON_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub host: String,
    pub id: String,
    pub name: String,
    pub max_inputs: u16,
    pub max_outputs: u16,
    pub sample_rates: Vec<u32>,
}

/// Every device on every host this build supports.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipping host {}: {e}", host_id.name());
                continue;
            }
        };
        for device in host.devices()? {
            let ins: Vec<_> = device
                .supported_input_configs()
                .map(|c| c.collect())
                .unwrap_or_default();
            let outs: Vec<_> = device
                .supported_output_configs()
                .map(|c| c.collect())
                .unwrap_or_default();
            out.push(DeviceInfo {
                host: host_id.name().to_string(),
                id: device.id().map(|i| i.to_string()).unwrap_or_default(),
                name: device_name(&device),
                max_inputs: ins.iter().map(|c| c.channels()).max().unwrap_or(0),
                max_outputs: outs.iter().map(|c| c.channels()).max().unwrap_or(0),
                sample_rates: rates(ins.iter().chain(outs.iter())),
            });
        }
    }
    Ok(out)
}

/// Pick a host by name ("wasapi", "asio"), or the platform default.
pub fn host(name: Option<&str>) -> Result<Host> {
    let Some(name) = name else {
        return Ok(cpal::default_host());
    };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let have: Vec<_> = cpal::available_hosts().iter().map(|h| h.name()).collect();
            anyhow!(
                "audio host '{name}' is not available in this build (available: {have:?}). \
                 ASIO needs `--features asio`."
            )
        })?;
    Ok(cpal::host_from_id(id)?)
}

/// Find an input or output device by (part of) its name or its exact id.
/// `None` means the host's default device.
pub fn find_device(host: &Host, query: Option<&str>, input: bool) -> Result<Device> {
    let kind = if input { "input" } else { "output" };
    let Some(query) = query else {
        let dev = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
        return dev.with_context(|| format!("no default {kind} device"));
    };
    let q = query.to_lowercase();
    let candidates: Vec<Device> = if input {
        host.input_devices()?.collect()
    } else {
        host.output_devices()?.collect()
    };
    candidates
        .into_iter()
        .find(|d| {
            let id = d.id().map(|i| i.to_string()).unwrap_or_default();
            id == query || device_name(d).to_lowercase().contains(&q)
        })
        .with_context(|| format!("no {kind} device matching '{query}' (run `qtdaw-probe devices`)"))
}

pub fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".into())
}

fn rates<'a>(configs: impl Iterator<Item = &'a SupportedStreamConfigRange>) -> Vec<u32> {
    let configs: Vec<_> = configs.collect();
    COMMON_RATES
        .into_iter()
        .filter(|r| configs.iter().any(|c| c.contains_rate(*r)))
        .collect()
}
