//! A quick look at the microphone: open the device `[audio] device` names and
//! read its format, or say why not and list what there is instead.

use crate::audio::device::InputDeviceInfo;

/// The device that opened, and its own format (whisrs resamples it to
/// 16 kHz mono).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
}

/// Why it did not open, and the inputs the sound server lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError {
    pub error: String,
    pub devices: Vec<InputDeviceInfo>,
}

/// Open `device` (`default`, or a source name) and read its format.
pub fn probe(device: &str) -> Result<Probe, ProbeError> {
    probe_with(device, open, crate::audio::device::list_input_devices)
}

/// [`probe`] with the opening and the listing passed in.
pub fn probe_with(
    device: &str,
    open: impl Fn(&str) -> Result<Probe, String>,
    list: impl Fn() -> Vec<InputDeviceInfo>,
) -> Result<Probe, ProbeError> {
    open(device).map_err(|error| ProbeError {
        error,
        devices: list(),
    })
}

/// Resolve `device` as dictation does and ask it for its input format.
fn open(device: &str) -> Result<Probe, String> {
    use cpal::traits::DeviceTrait;
    let resolved = crate::audio::device::resolve_input(device).map_err(|e| format!("{e:#}"))?;
    let config = resolved
        .device
        .default_input_config()
        .map_err(|e| format!("could not query the device's format: {e}"))?;
    Ok(Probe {
        name: resolved.device.to_string(),
        rate: config.sample_rate(),
        channels: config.channels(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::device::DeviceHost;

    #[test]
    fn a_device_that_does_not_open_lists_the_others() {
        let yeti = || {
            vec![InputDeviceInfo {
                host: DeviceHost::PulseAudio,
                id: "alsa_input.usb-yeti".into(),
                description: "Blue Yeti".into(),
            }]
        };
        let opened = probe_with(
            "default",
            |name| {
                Ok(Probe {
                    name: format!("{name} mic"),
                    rate: 48_000,
                    channels: 2,
                })
            },
            Vec::new,
        );
        assert_eq!(opened.unwrap().name, "default mic");
        let failed = probe_with("nope", |_| Err("no such device".into()), yeti).unwrap_err();
        assert_eq!(failed.error, "no such device");
        assert_eq!(failed.devices[0].description, "Blue Yeti");
    }
}
