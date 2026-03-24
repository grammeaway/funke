use std::collections::HashMap;
use std::process::Stdio;

use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use zbus::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// Information about a BlueZ adapter.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub name: String,
    pub address: String,
    pub powered: bool,
}

/// Information about a Bluetooth device.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: Option<String>,
    pub address: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub icon: Option<String>,
    pub uuids: Vec<String>,
}

impl DeviceInfo {
    /// Returns the display name: the device name if available, otherwise the address.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.address)
    }

    /// Returns true if the device has audio-related Bluetooth UUIDs.
    pub fn has_audio_profiles(&self) -> bool {
        const AUDIO_UUID_PREFIXES: &[&str] = &[
            "0000110a", // A2DP Source
            "0000110b", // A2DP Sink
            "00001108", // HSP HS
            "00001112", // HSP AG
            "0000111e", // HFP HF
            "0000111f", // HFP AG
        ];
        self.uuids.iter().any(|uuid| {
            let lower = uuid.to_lowercase();
            AUDIO_UUID_PREFIXES
                .iter()
                .any(|prefix| lower.starts_with(prefix))
        })
    }
}

/// Proxy for the org.bluez.Adapter1 D-Bus interface.
#[zbus::proxy(
    interface = "org.bluez.Adapter1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0"
)]
trait Adapter1 {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn set_powered(&self, value: bool) -> zbus::Result<()>;

    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn remove_device(&self, device: &OwnedObjectPath) -> zbus::Result<()>;
}

/// Proxy for the org.bluez.Device1 D-Bus interface.
#[zbus::proxy(
    interface = "org.bluez.Device1",
    default_service = "org.bluez"
)]
trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn pair(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn set_trusted(&self, value: bool) -> zbus::Result<()>;
}

/// Proxy for the org.bluez.AgentManager1 D-Bus interface.
#[zbus::proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
trait AgentManager1 {
    fn register_agent(&self, agent: &OwnedObjectPath, capability: &str) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &OwnedObjectPath) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &OwnedObjectPath) -> zbus::Result<()>;
}

/// A request from the BlueZ pairing agent to the TUI for user interaction.
pub enum AgentRequest {
    RequestPinCode {
        device: String,
        reply: oneshot::Sender<Option<String>>,
    },
    RequestPasskey {
        device: String,
        reply: oneshot::Sender<Option<u32>>,
    },
    DisplayPasskey {
        device: String,
        passkey: u32,
    },
    RequestConfirmation {
        device: String,
        passkey: u32,
        reply: oneshot::Sender<bool>,
    },
    AuthorizeService {
        device: String,
        uuid: String,
        reply: oneshot::Sender<bool>,
    },
    Cancel,
}

/// The BlueZ Agent1 D-Bus interface implementation.
pub struct PairingAgent {
    tx: UnboundedSender<AgentRequest>,
}

impl PairingAgent {
    pub fn new(tx: UnboundedSender<AgentRequest>) -> Self {
        Self { tx }
    }
}

/// The D-Bus object path for our pairing agent.
pub const AGENT_PATH: &str = "/org/bluez/funke_agent";

#[zbus::interface(name = "org.bluez.Agent1")]
impl PairingAgent {
    async fn release(&self) {}

    async fn request_pin_code(
        &self,
        device: OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let device_str = device.to_string();
        let _ = self.tx.send(AgentRequest::RequestPinCode {
            device: device_str,
            reply: reply_tx,
        });
        match reply_rx.await {
            Ok(Some(pin)) => Ok(pin),
            _ => Err(zbus::fdo::Error::Failed("PIN entry cancelled".to_string())),
        }
    }

    async fn request_passkey(
        &self,
        device: OwnedObjectPath,
    ) -> zbus::fdo::Result<u32> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let device_str = device.to_string();
        let _ = self.tx.send(AgentRequest::RequestPasskey {
            device: device_str,
            reply: reply_tx,
        });
        match reply_rx.await {
            Ok(Some(passkey)) => Ok(passkey),
            _ => Err(zbus::fdo::Error::Failed("Passkey entry cancelled".to_string())),
        }
    }

    async fn display_passkey(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        _entered: u16,
    ) {
        let device_str = device.to_string();
        let _ = self.tx.send(AgentRequest::DisplayPasskey {
            device: device_str,
            passkey,
        });
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let device_str = device.to_string();
        let _ = self.tx.send(AgentRequest::RequestConfirmation {
            device: device_str,
            passkey,
            reply: reply_tx,
        });
        match reply_rx.await {
            Ok(true) => Ok(()),
            _ => Err(zbus::fdo::Error::Failed("Confirmation rejected".to_string())),
        }
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        uuid: String,
    ) -> zbus::fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let device_str = device.to_string();
        let _ = self.tx.send(AgentRequest::AuthorizeService {
            device: device_str,
            uuid,
            reply: reply_tx,
        });
        match reply_rx.await {
            Ok(true) => Ok(()),
            _ => Err(zbus::fdo::Error::Failed("Service not authorized".to_string())),
        }
    }

    async fn cancel(&self) {
        let _ = self.tx.send(AgentRequest::Cancel);
    }
}

/// Register the pairing agent with BlueZ.
pub async fn register_agent(connection: &Connection, tx: UnboundedSender<AgentRequest>) -> Result<(), zbus::Error> {
    let agent = PairingAgent::new(tx);
    let agent_path = OwnedObjectPath::try_from(AGENT_PATH.to_string())
        .map_err(|e| zbus::Error::Address(e.to_string()))?;

    // Serve the agent object on D-Bus
    connection.object_server().at(AGENT_PATH, agent).await?;

    // Register with BlueZ AgentManager
    let proxy = AgentManager1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.register_agent(&agent_path, "KeyboardDisplay").await?;
    proxy.request_default_agent(&agent_path).await?;

    Ok(())
}

/// Unregister the pairing agent from BlueZ.
pub async fn unregister_agent(connection: &Connection) -> Result<(), zbus::Error> {
    let agent_path = OwnedObjectPath::try_from(AGENT_PATH.to_string())
        .map_err(|e| zbus::Error::Address(e.to_string()))?;

    let proxy = AgentManager1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let _ = proxy.unregister_agent(&agent_path).await;

    // Remove the agent object from the connection
    let _ = connection.object_server().remove::<PairingAgent, _>(AGENT_PATH).await;

    Ok(())
}

/// Proxy for the org.freedesktop.DBus.ObjectManager interface on BlueZ.
#[zbus::proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.bluez",
    default_path = "/"
)]
trait ObjectManager {
    fn get_managed_objects(
        &self,
    ) -> zbus::Result<
        HashMap<
            OwnedObjectPath,
            HashMap<String, HashMap<String, OwnedValue>>,
        >,
    >;

    #[zbus(signal)]
    fn interfaces_added(
        &self,
        object_path: OwnedObjectPath,
        interfaces_and_properties: HashMap<String, HashMap<String, OwnedValue>>,
    ) -> zbus::Result<()>;
}

/// Connect to system D-Bus and retrieve the default BlueZ adapter info.
pub async fn get_adapter_info(connection: &Connection) -> Result<AdapterInfo, zbus::Error> {
    let proxy = Adapter1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let name = proxy.name().await?;
    let address = proxy.address().await?;
    let powered = proxy.powered().await?;

    Ok(AdapterInfo { name, address, powered })
}

/// Returns true if the error indicates the adapter D-Bus object does not exist.
fn is_adapter_missing(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, _, _) => {
            let s = name.as_str();
            s == "org.freedesktop.DBus.Error.UnknownObject"
                || s == "org.freedesktop.DBus.Error.ServiceUnknown"
        }
        zbus::Error::FDO(fdo) => matches!(
            fdo.as_ref(),
            zbus::fdo::Error::UnknownObject(_)
                | zbus::fdo::Error::ServiceUnknown(_)
                | zbus::fdo::Error::UnknownInterface(_)
        ),
        // InterfaceNotFound can occur when BlueZ hasn't registered the adapter
        zbus::Error::InterfaceNotFound => true,
        // Fallback: check the Display string for known D-Bus error names
        e => {
            let msg = e.to_string();
            msg.contains("UnknownObject") || msg.contains("ServiceUnknown")
        }
    }
}

/// Try to retrieve BlueZ adapter info, returning `None` if the adapter
/// D-Bus object does not exist (e.g. adapter hardware is off).
pub async fn try_get_adapter_info(connection: &Connection) -> Result<Option<AdapterInfo>, zbus::Error> {
    match get_adapter_info(connection).await {
        Ok(info) => Ok(Some(info)),
        Err(e) if is_adapter_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Fetch known (previously paired) devices from BlueZ via ObjectManager.
pub async fn get_known_devices(connection: &Connection) -> Result<Vec<DeviceInfo>, zbus::Error> {
    let proxy = ObjectManagerProxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let objects = proxy.get_managed_objects().await?;
    let devices = parse_devices_from_objects(&objects);
    Ok(devices)
}

/// Parse a single device from a D-Bus interfaces map (used for both ObjectManager
/// enumeration and InterfacesAdded signals).
pub fn parse_device_from_interfaces(
    interfaces: &HashMap<String, HashMap<String, OwnedValue>>,
) -> Option<DeviceInfo> {
    let props = interfaces.get("org.bluez.Device1")?;

    let address = props
        .get("Address")
        .and_then(|v| <String>::try_from(v.clone()).ok())
        .unwrap_or_default();

    let name = props
        .get("Name")
        .and_then(|v| <String>::try_from(v.clone()).ok());

    let paired = props
        .get("Paired")
        .and_then(|v| <bool>::try_from(v.clone()).ok())
        .unwrap_or(false);

    let connected = props
        .get("Connected")
        .and_then(|v| <bool>::try_from(v.clone()).ok())
        .unwrap_or(false);

    let trusted = props
        .get("Trusted")
        .and_then(|v| <bool>::try_from(v.clone()).ok())
        .unwrap_or(false);

    let icon = props
        .get("Icon")
        .and_then(|v| <String>::try_from(v.clone()).ok());

    let uuids = props
        .get("UUIDs")
        .and_then(|v| <Vec<String>>::try_from(v.clone()).ok())
        .unwrap_or_default();

    Some(DeviceInfo {
        name,
        address,
        paired,
        connected,
        trusted,
        icon,
        uuids,
    })
}

/// Parse device information from ObjectManager's managed objects.
pub fn parse_devices_from_objects(
    objects: &HashMap<
        OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    >,
) -> Vec<DeviceInfo> {
    let mut devices: Vec<DeviceInfo> = objects
        .iter()
        .filter(|(path, _)| path.as_str().starts_with("/org/bluez/"))
        .filter_map(|(_, interfaces)| parse_device_from_interfaces(interfaces))
        .collect();

    // Sort: connected first, then paired, then by display name
    devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then(b.paired.cmp(&a.paired))
            .then(a.display_name().to_lowercase().cmp(&b.display_name().to_lowercase()))
    });

    devices
}

/// Start Bluetooth discovery on the default adapter.
pub async fn start_discovery(connection: &Connection) -> Result<(), zbus::Error> {
    let proxy = Adapter1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.start_discovery().await
}

/// Stop Bluetooth discovery on the default adapter.
pub async fn stop_discovery(connection: &Connection) -> Result<(), zbus::Error> {
    let proxy = Adapter1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.stop_discovery().await
}

/// Watch for newly discovered devices via D-Bus InterfacesAdded signals.
/// Sends discovered DeviceInfo through the provided channel.
pub async fn watch_device_discoveries(
    connection: &Connection,
    tx: tokio::sync::mpsc::UnboundedSender<DeviceInfo>,
) -> Result<(), zbus::Error> {
    use futures_util::StreamExt;

    let proxy = ObjectManagerProxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let mut stream = proxy.receive_interfaces_added().await?;

    while let Some(signal) = stream.next().await {
        if let Ok(args) = signal.args()
            && let Some(device) = parse_device_from_interfaces(args.interfaces_and_properties())
            && tx.send(device).is_err()
        {
            break;
        }
    }

    Ok(())
}

/// Construct the BlueZ D-Bus object path for a device given its address.
fn device_object_path(address: &str) -> String {
    format!("/org/bluez/hci0/dev_{}", address.replace(':', "_"))
}

/// Connect to a Bluetooth device by address.
pub async fn connect_device(connection: &Connection, address: &str) -> Result<(), zbus::Error> {
    let path = device_object_path(address);
    let proxy = Device1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .path(path)?
        .build()
        .await?;
    proxy.connect().await
}

/// Disconnect from a Bluetooth device by address.
pub async fn disconnect_device(connection: &Connection, address: &str) -> Result<(), zbus::Error> {
    let path = device_object_path(address);
    let proxy = Device1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .path(path)?
        .build()
        .await?;
    proxy.disconnect().await
}

/// Pair with a Bluetooth device by address.
pub async fn pair_device(connection: &Connection, address: &str) -> Result<(), zbus::Error> {
    let path = device_object_path(address);
    let proxy = Device1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .path(path)?
        .build()
        .await?;
    proxy.pair().await
}

/// Remove (unpair) a Bluetooth device by address via the adapter.
pub async fn remove_device(connection: &Connection, address: &str) -> Result<(), zbus::Error> {
    let path = device_object_path(address);
    let obj_path = OwnedObjectPath::try_from(path).map_err(|e| zbus::Error::Address(e.to_string()))?;
    let proxy = Adapter1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.remove_device(&obj_path).await
}

/// Set the trusted property of a Bluetooth device by address.
pub async fn set_device_trusted(connection: &Connection, address: &str, trusted: bool) -> Result<(), zbus::Error> {
    let path = device_object_path(address);
    let proxy = Device1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .path(path)?
        .build()
        .await?;
    proxy.set_trusted(trusted).await
}

/// Power on the Bluetooth adapter.
pub async fn power_on_adapter(connection: &Connection) -> Result<(), zbus::Error> {
    let proxy = Adapter1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.set_powered(true).await
}

/// Power off the Bluetooth adapter.
pub async fn power_off_adapter(connection: &Connection) -> Result<(), zbus::Error> {
    let proxy = Adapter1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    proxy.set_powered(false).await
}

/// Unblock the Bluetooth adapter via rfkill.
pub async fn rfkill_unblock_bluetooth() -> Result<(), String> {
    let output = Command::new("rfkill")
        .args(["unblock", "bluetooth"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run rfkill: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("rfkill unblock failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Connect to the system D-Bus.
pub async fn connect_system_dbus() -> Result<Connection, zbus::Error> {
    Connection::system().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    #[test]
    fn test_adapter_info_clone_and_debug() {
        let info = AdapterInfo {
            name: "hci0".to_string(),
            address: "00:11:22:33:44:55".to_string(),
            powered: true,
        };
        let cloned = info.clone();
        assert_eq!(cloned.name, "hci0");
        assert_eq!(cloned.address, "00:11:22:33:44:55");
        assert!(cloned.powered);
        let debug = format!("{:?}", info);
        assert!(debug.contains("hci0"));
    }

    #[test]
    fn test_device_info_display_name_with_name() {
        let device = DeviceInfo {
            name: Some("My Speaker".to_string()),
            address: "11:22:33:44:55:66".to_string(),
            paired: true,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        assert_eq!(device.display_name(), "My Speaker");
    }

    #[test]
    fn test_device_info_display_name_without_name() {
        let device = DeviceInfo {
            name: None,
            address: "11:22:33:44:55:66".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        assert_eq!(device.display_name(), "11:22:33:44:55:66");
    }

    #[test]
    fn test_parse_devices_from_objects_empty() {
        let objects = HashMap::new();
        let devices = parse_devices_from_objects(&objects);
        assert!(devices.is_empty());
    }

    fn make_device_props(
        name: Option<&str>,
        address: &str,
        paired: bool,
        connected: bool,
    ) -> HashMap<String, OwnedValue> {
        let mut props = HashMap::new();
        if let Some(n) = name {
            props.insert("Name".to_string(), Value::from(n.to_string()).try_into().unwrap());
        }
        props.insert("Address".to_string(), Value::from(address.to_string()).try_into().unwrap());
        props.insert("Paired".to_string(), Value::from(paired).try_into().unwrap());
        props.insert("Connected".to_string(), Value::from(connected).try_into().unwrap());
        props.insert("Trusted".to_string(), Value::from(false).try_into().unwrap());
        props
    }

    #[test]
    fn test_parse_devices_from_objects() {
        let mut objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
            HashMap::new();

        let mut ifaces1 = HashMap::new();
        ifaces1.insert(
            "org.bluez.Device1".to_string(),
            make_device_props(Some("Speaker"), "AA:BB:CC:DD:EE:01", true, false),
        );
        objects.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01").unwrap(),
            ifaces1,
        );

        let mut ifaces2 = HashMap::new();
        ifaces2.insert(
            "org.bluez.Device1".to_string(),
            make_device_props(None, "AA:BB:CC:DD:EE:02", false, false),
        );
        objects.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02").unwrap(),
            ifaces2,
        );

        let mut ifaces3 = HashMap::new();
        ifaces3.insert(
            "org.bluez.Device1".to_string(),
            make_device_props(Some("Headphones"), "AA:BB:CC:DD:EE:03", true, true),
        );
        objects.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_03").unwrap(),
            ifaces3,
        );

        let devices = parse_devices_from_objects(&objects);
        assert_eq!(devices.len(), 3);

        // Connected device should be first
        assert_eq!(devices[0].display_name(), "Headphones");
        assert!(devices[0].connected);

        // Paired device second
        assert_eq!(devices[1].display_name(), "Speaker");
        assert!(devices[1].paired);
        assert!(!devices[1].connected);
    }

    #[test]
    fn test_parse_devices_ignores_non_device_interfaces() {
        let mut objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
            HashMap::new();

        // An object with only Adapter1 interface, no Device1
        let mut ifaces = HashMap::new();
        ifaces.insert("org.bluez.Adapter1".to_string(), HashMap::new());
        objects.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0").unwrap(),
            ifaces,
        );

        let devices = parse_devices_from_objects(&objects);
        assert!(devices.is_empty());
    }

    #[test]
    fn test_parse_device_from_interfaces_with_device1() {
        let mut interfaces = HashMap::new();
        interfaces.insert(
            "org.bluez.Device1".to_string(),
            make_device_props(Some("Headset"), "11:22:33:44:55:66", false, false),
        );
        let device = parse_device_from_interfaces(&interfaces).unwrap();
        assert_eq!(device.display_name(), "Headset");
        assert_eq!(device.address, "11:22:33:44:55:66");
        assert!(!device.paired);
        assert!(!device.connected);
    }

    #[test]
    fn test_parse_device_from_interfaces_without_device1() {
        let mut interfaces = HashMap::new();
        interfaces.insert("org.bluez.Adapter1".to_string(), HashMap::new());
        assert!(parse_device_from_interfaces(&interfaces).is_none());
    }

    #[test]
    fn test_parse_device_from_interfaces_empty() {
        let interfaces = HashMap::new();
        assert!(parse_device_from_interfaces(&interfaces).is_none());
    }

    #[test]
    fn test_parse_device_from_interfaces_with_extended_props() {
        let mut props = make_device_props(Some("Speaker"), "AA:BB:CC:DD:EE:01", true, true);
        props.insert("Trusted".to_string(), Value::from(true).try_into().unwrap());
        props.insert("Icon".to_string(), Value::from("audio-card".to_string()).try_into().unwrap());
        let uuids: Vec<String> = vec!["0000110b-0000-1000-8000-00805f9b34fb".to_string()];
        props.insert("UUIDs".to_string(), Value::from(uuids.clone()).try_into().unwrap());

        let mut interfaces = HashMap::new();
        interfaces.insert("org.bluez.Device1".to_string(), props);

        let device = parse_device_from_interfaces(&interfaces).unwrap();
        assert!(device.trusted);
        assert_eq!(device.icon.as_deref(), Some("audio-card"));
        assert_eq!(device.uuids, uuids);
    }

    #[test]
    fn test_parse_device_from_interfaces_defaults_extended_props() {
        let mut interfaces = HashMap::new();
        interfaces.insert(
            "org.bluez.Device1".to_string(),
            make_device_props(Some("Headset"), "11:22:33:44:55:66", false, false),
        );
        let device = parse_device_from_interfaces(&interfaces).unwrap();
        assert!(!device.trusted);
        assert!(device.icon.is_none());
        assert!(device.uuids.is_empty());
    }

    #[test]
    fn test_has_audio_profiles_true() {
        let device = DeviceInfo {
            name: Some("Speaker".to_string()),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            paired: true,
            connected: true,
            trusted: false,
            icon: Some("audio-card".to_string()),
            uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".to_string()],
        };
        assert!(device.has_audio_profiles());
    }

    #[test]
    fn test_has_audio_profiles_false() {
        let device = DeviceInfo {
            name: Some("Keyboard".to_string()),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            paired: true,
            connected: false,
            trusted: false,
            icon: Some("input-keyboard".to_string()),
            uuids: vec!["00001124-0000-1000-8000-00805f9b34fb".to_string()],
        };
        assert!(!device.has_audio_profiles());
    }

    #[test]
    fn test_has_audio_profiles_empty_uuids() {
        let device = DeviceInfo {
            name: Some("Unknown".to_string()),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        assert!(!device.has_audio_profiles());
    }

    #[test]
    fn test_has_audio_profiles_multiple_uuids() {
        let device = DeviceInfo {
            name: Some("Headset".to_string()),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            paired: true,
            connected: true,
            trusted: false,
            icon: None,
            uuids: vec![
                "00001200-0000-1000-8000-00805f9b34fb".to_string(), // non-audio
                "0000111e-0000-1000-8000-00805f9b34fb".to_string(), // HFP HF
            ],
        };
        assert!(device.has_audio_profiles());
    }

    #[test]
    fn test_device_object_path() {
        assert_eq!(
            device_object_path("AA:BB:CC:DD:EE:FF"),
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
        );
        assert_eq!(
            device_object_path("11:22:33:44:55:66"),
            "/org/bluez/hci0/dev_11_22_33_44_55_66"
        );
    }

    #[tokio::test]
    async fn test_connect_device_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                // Try connecting to a non-existent device — should fail gracefully
                let result = connect_device(&conn, "00:00:00:00:00:00").await;
                assert!(result.is_err());
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_disconnect_device_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                let result = disconnect_device(&conn, "00:00:00:00:00:00").await;
                assert!(result.is_err());
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_pair_device_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                let result = pair_device(&conn, "00:00:00:00:00:00").await;
                assert!(result.is_err());
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_remove_device_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                let result = remove_device(&conn, "00:00:00:00:00:00").await;
                assert!(result.is_err());
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_set_device_trusted_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                let result = set_device_trusted(&conn, "00:00:00:00:00:00", true).await;
                assert!(result.is_err());
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_connect_system_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                match get_adapter_info(&conn).await {
                    Ok(info) => {
                        assert!(!info.name.is_empty());
                        assert!(!info.address.is_empty());
                    }
                    Err(e) => {
                        eprintln!("Adapter not available (expected in CI): {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_get_known_devices() {
        match connect_system_dbus().await {
            Ok(conn) => {
                match get_known_devices(&conn).await {
                    Ok(devices) => {
                        // Verify all devices have addresses
                        for device in &devices {
                            assert!(!device.address.is_empty());
                        }
                    }
                    Err(e) => {
                        eprintln!("Could not fetch devices (expected in CI): {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[test]
    fn test_agent_path_constant() {
        assert_eq!(AGENT_PATH, "/org/bluez/funke_agent");
    }

    #[test]
    fn test_pairing_agent_new() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let _agent = PairingAgent::new(tx);
    }

    #[tokio::test]
    async fn test_register_agent_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                // May succeed or fail depending on BlueZ state
                let result = register_agent(&conn, tx).await;
                if result.is_ok() {
                    // Clean up
                    let _ = unregister_agent(&conn).await;
                }
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_agent_request_pin_code_cancel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentRequest>();
        let agent = PairingAgent::new(tx);

        let agent_task = tokio::spawn(async move {
            let path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB".to_string()).unwrap();
            agent.request_pin_code(path).await
        });

        // Receive the request from the agent
        if let Some(AgentRequest::RequestPinCode { reply, .. }) = rx.recv().await {
            // Simulate cancel
            let _ = reply.send(None);
        }

        let result = agent_task.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_agent_request_pin_code_success() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentRequest>();
        let agent = PairingAgent::new(tx);

        let agent_task = tokio::spawn(async move {
            let path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB".to_string()).unwrap();
            agent.request_pin_code(path).await
        });

        if let Some(AgentRequest::RequestPinCode { reply, .. }) = rx.recv().await {
            let _ = reply.send(Some("1234".to_string()));
        }

        let result = agent_task.await.unwrap();
        assert_eq!(result.unwrap(), "1234");
    }

    #[tokio::test]
    async fn test_agent_request_confirmation_accept() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentRequest>();
        let agent = PairingAgent::new(tx);

        let agent_task = tokio::spawn(async move {
            let path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB".to_string()).unwrap();
            agent.request_confirmation(path, 123456).await
        });

        if let Some(AgentRequest::RequestConfirmation { reply, passkey, .. }) = rx.recv().await {
            assert_eq!(passkey, 123456);
            let _ = reply.send(true);
        }

        let result = agent_task.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_agent_request_confirmation_reject() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentRequest>();
        let agent = PairingAgent::new(tx);

        let agent_task = tokio::spawn(async move {
            let path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB".to_string()).unwrap();
            agent.request_confirmation(path, 123456).await
        });

        if let Some(AgentRequest::RequestConfirmation { reply, .. }) = rx.recv().await {
            let _ = reply.send(false);
        }

        let result = agent_task.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_agent_cancel_sends_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentRequest>();
        let agent = PairingAgent::new(tx);

        agent.cancel().await;

        if let Some(request) = rx.recv().await {
            assert!(matches!(request, AgentRequest::Cancel));
        } else {
            panic!("Expected Cancel request");
        }
    }

    #[tokio::test]
    async fn test_power_on_adapter_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                // May succeed or fail depending on adapter state/permissions
                let _result = power_on_adapter(&conn).await;
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }

    #[tokio::test]
    async fn test_power_off_adapter_no_dbus() {
        match connect_system_dbus().await {
            Ok(conn) => {
                let _result = power_off_adapter(&conn).await;
            }
            Err(e) => {
                eprintln!("D-Bus not available (expected in CI): {e}");
            }
        }
    }
}
