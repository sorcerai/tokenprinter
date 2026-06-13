use anyhow::{anyhow, Context};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode { Auto, Cups, Usb }
impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.to_ascii_lowercase().as_str() { "cups"=>Mode::Cups, "usb"=>Mode::Usb, _=>Mode::Auto }
    }
}

const STAR_VID: u16 = 0x0519;

/// Send raw bytes to the printer. Honors `mode`; Auto = CUPS then USB.
pub fn send(bytes: &[u8], mode: Mode, queue: &str) -> anyhow::Result<()> {
    match mode {
        Mode::Cups => send_cups(bytes, queue),
        Mode::Usb => send_usb(bytes),
        Mode::Auto => send_cups(bytes, queue).or_else(|e| {
            eprintln!("cups failed ({e}); trying usb");
            send_usb(bytes)
        }),
    }
}

fn send_cups(bytes: &[u8], queue: &str) -> anyhow::Result<()> {
    let mut child = Command::new("lp")
        .args(["-d", queue, "-o", "raw"])
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().context("spawn lp (is CUPS installed?)")?;
    child.stdin.as_mut().ok_or_else(|| anyhow!("no stdin"))?.write_all(bytes)?;
    // Drop stdin so lp sees EOF, then wait with a hard timeout.
    drop(child.stdin.take());
    match child.wait_timeout(Duration::from_secs(30))? {
        Some(status) => {
            let out = child.wait_with_output()?;
            if !status.success() {
                return Err(anyhow!("lp failed: {}", String::from_utf8_lossy(&out.stderr)));
            }
            Ok(())
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(anyhow!("lp timed out after 30s — CUPS queue may be stuck"))
        }
    }
}

fn send_usb(bytes: &[u8]) -> anyhow::Result<()> {
    let dh = rusb::open_device_with_vid_pid(STAR_VID, find_pid()?)
        .ok_or_else(|| anyhow!("Star printer (vid 0x0519) not found on USB"))?;
    let _ = dh.set_auto_detach_kernel_driver(true);
    dh.claim_interface(0).context("claim usb interface 0 (in use by CUPS?)")?;
    let ep = bulk_out_endpoint().unwrap_or(0x01);
    dh.write_bulk(ep, bytes, std::time::Duration::from_secs(5))?;
    let _ = dh.release_interface(0);
    Ok(())
}

fn find_pid() -> anyhow::Result<u16> {
    for d in rusb::devices()?.iter() {
        if let Ok(desc) = d.device_descriptor() {
            if desc.vendor_id() == STAR_VID { return Ok(desc.product_id()); }
        }
    }
    Err(anyhow!("no Star USB device"))
}

fn bulk_out_endpoint() -> Option<u8> {
    let devices = rusb::devices().ok()?;
    for d in devices.iter() {
        let desc = d.device_descriptor().ok()?;
        if desc.vendor_id() != STAR_VID { continue; }
        let cfg = d.active_config_descriptor().ok()?;
        for iface in cfg.interfaces() {
            for id in iface.descriptors() {
                for ep in id.endpoint_descriptors() {
                    if ep.direction() == rusb::Direction::Out
                        && ep.transfer_type() == rusb::TransferType::Bulk {
                        return Some(ep.address());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("cups"), Mode::Cups);
        assert_eq!(Mode::parse("usb"), Mode::Usb);
        assert_eq!(Mode::parse("auto"), Mode::Auto);
        assert_eq!(Mode::parse("nonsense"), Mode::Auto);
    }
}
