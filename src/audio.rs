use std::process::Stdio;
use tokio::process::Command;

/// An audio profile available for a Bluetooth device.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioProfile {
    pub name: String,
    pub description: String,
    pub active: bool,
}

/// Get available audio profiles for a Bluetooth device by address.
pub async fn get_device_profiles(address: &str) -> Result<Vec<AudioProfile>, String> {
    let card_name = format!("bluez_card.{}", address.replace(':', "_"));

    let output = Command::new("pactl")
        .args(["list", "cards"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run pactl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pactl failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_card_profiles(&stdout, &card_name)
}

/// Set the audio profile for a Bluetooth device by address.
pub async fn set_card_profile(address: &str, profile: &str) -> Result<(), String> {
    let card_name = format!("bluez_card.{}", address.replace(':', "_"));

    let output = Command::new("pactl")
        .args(["set-card-profile", &card_name, profile])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run pactl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Profile switch failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Parse audio profiles from `pactl list cards` output for a specific card.
pub fn parse_card_profiles(
    pactl_output: &str,
    card_name: &str,
) -> Result<Vec<AudioProfile>, String> {
    let mut in_target_card = false;
    let mut in_profiles = false;
    let mut active_profile = String::new();
    let mut profiles = Vec::new();

    for line in pactl_output.lines() {
        if line.starts_with("Card #") {
            in_target_card = false;
            in_profiles = false;
        }

        if line.contains("Name:") && line.contains(card_name) {
            in_target_card = true;
            continue;
        }

        if !in_target_card {
            continue;
        }

        let trimmed = line.trim();

        if trimmed == "Profiles:" {
            in_profiles = true;
            continue;
        }

        if trimmed.starts_with("Active Profile:") {
            active_profile = trimmed
                .strip_prefix("Active Profile:")
                .unwrap_or("")
                .trim()
                .to_string();
            break;
        }

        if in_profiles && !trimmed.is_empty() {
            // Profile lines are indented; a non-indented line ends the section
            if !line.starts_with('\t') && !line.starts_with("  ") {
                in_profiles = false;
                continue;
            }

            if let Some((name, rest)) = trimmed.split_once(": ") {
                if name == "off" {
                    continue;
                }
                let description = if let Some(pos) = rest.find(" (sinks:") {
                    &rest[..pos]
                } else {
                    rest
                };
                profiles.push(AudioProfile {
                    name: name.to_string(),
                    description: description.trim().to_string(),
                    active: false,
                });
            }
        }
    }

    if profiles.is_empty() {
        return Err("No audio profiles found".to_string());
    }

    for profile in &mut profiles {
        if profile.name == active_profile {
            profile.active = true;
        }
    }

    Ok(profiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL_OUTPUT: &str = "\
Card #46
\tName: bluez_card.AA_BB_CC_DD_EE_FF
\tDriver: module-bluez5-device.c
\tOwner Module: 23
\tProperties:
\t\tdevice.string = \"AA:BB:CC:DD:EE:FF\"
\tProfiles:
\t\ta2dp-sink: High Fidelity Playback (A2DP Sink, codec SBC) (sinks: 1, sources: 0, priority: 40, available: yes)
\t\theadset-head-unit: Headset Head Unit (HSP/HFP) (sinks: 1, sources: 1, priority: 30, available: yes)
\t\ta2dp-sink-aac: High Fidelity Playback (A2DP Sink, codec AAC) (sinks: 1, sources: 0, priority: 40, available: yes)
\t\toff: Off (sinks: 0, sources: 0, priority: 0, available: yes)
\tActive Profile: a2dp-sink
\tPorts:
\t\tspeaker-output: Speaker (type: Speaker, priority: 0, latency offset: 0 usec, available)
";

    #[test]
    fn test_parse_card_profiles() {
        let profiles =
            parse_card_profiles(PACTL_OUTPUT, "bluez_card.AA_BB_CC_DD_EE_FF").unwrap();
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].name, "a2dp-sink");
        assert_eq!(
            profiles[0].description,
            "High Fidelity Playback (A2DP Sink, codec SBC)"
        );
        assert!(profiles[0].active);
        assert_eq!(profiles[1].name, "headset-head-unit");
        assert_eq!(profiles[1].description, "Headset Head Unit (HSP/HFP)");
        assert!(!profiles[1].active);
        assert_eq!(profiles[2].name, "a2dp-sink-aac");
        assert!(!profiles[2].active);
    }

    #[test]
    fn test_parse_card_profiles_no_card() {
        let result = parse_card_profiles(PACTL_OUTPUT, "bluez_card.11_22_33_44_55_66");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No audio profiles found"));
    }

    #[test]
    fn test_parse_card_profiles_empty_output() {
        let result = parse_card_profiles("", "bluez_card.AA_BB_CC_DD_EE_FF");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_card_profiles_multiple_cards() {
        let output = format!(
            "\
Card #10
\tName: alsa_card.pci-0000_00_1f.3
\tProfiles:
\t\toutput:analog-stereo: Analog Stereo Output (sinks: 1, sources: 0, priority: 6500, available: yes)
\t\toff: Off (sinks: 0, sources: 0, priority: 0, available: yes)
\tActive Profile: output:analog-stereo

{}",
            PACTL_OUTPUT
        );
        let profiles =
            parse_card_profiles(&output, "bluez_card.AA_BB_CC_DD_EE_FF").unwrap();
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].name, "a2dp-sink");
    }

    #[test]
    fn test_parse_card_profiles_skips_off() {
        let profiles =
            parse_card_profiles(PACTL_OUTPUT, "bluez_card.AA_BB_CC_DD_EE_FF").unwrap();
        assert!(!profiles.iter().any(|p| p.name == "off"));
    }

    #[test]
    fn test_parse_card_profiles_active_marked() {
        let profiles =
            parse_card_profiles(PACTL_OUTPUT, "bluez_card.AA_BB_CC_DD_EE_FF").unwrap();
        let active: Vec<_> = profiles.iter().filter(|p| p.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "a2dp-sink");
    }
}
