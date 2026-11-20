use std::collections::HashMap;
use std::process;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::{Header, Type as MessageType};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::icon::IconStore;
use super::{Tray, TrayItem};
use crate::Service;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";

static SHARED_SESSION: OnceLock<Option<Connection>> = OnceLock::new();

pub fn shared_session() -> Option<Connection> {
    SHARED_SESSION
        .get_or_init(|| Connection::session().ok())
        .clone()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WatcherEvent {
    Registered(String),
    Unregistered(String),
    RefreshService(String),
    OwnerVanished(String),
}

/// Internal seam separating SNI registry logic from live session D-Bus I/O.
trait TrayBus {
    fn fetch_properties(&self, service: &str, path: &str) -> Option<HashMap<String, OwnedValue>>;
    fn emit_unregistered(&self, full_id: &str);
}

struct ZbusAdapter<'a> {
    connection: &'a Connection,
    owned_watcher: bool,
}

impl TrayBus for ZbusAdapter<'_> {
    fn fetch_properties(&self, service: &str, path: &str) -> Option<HashMap<String, OwnedValue>> {
        let proxy = Proxy::new(
            self.connection,
            service,
            path,
            "org.freedesktop.DBus.Properties",
        )
        .ok()?;
        proxy.call("GetAll", &(ITEM_IFACE)).ok()
    }

    fn emit_unregistered(&self, full_id: &str) {
        if !self.owned_watcher {
            return;
        }
        let Ok(iface_ref) = self
            .connection
            .object_server()
            .interface::<_, StatusNotifierWatcher>(WATCHER_PATH)
        else {
            return;
        };
        let emitter = iface_ref.signal_emitter();
        let _ = zbus::block_on(StatusNotifierWatcher::status_notifier_item_unregistered(
            emitter, full_id,
        ));
    }
}

/// Authoritative in-memory registry of active SNI items and their temp icons.
struct TrayRegistry {
    items: Vec<TrayItem>,
    icons: IconStore,
}

impl TrayRegistry {
    fn new(icons: IconStore) -> Self {
        Self {
            items: Vec::new(),
            icons,
        }
    }

    fn registered_ids(&self) -> Vec<String> {
        self.items.iter().map(|item| item.id.clone()).collect()
    }

    fn contains(&self, full_id: &str) -> bool {
        self.items.iter().any(|item| item.id == full_id)
    }

    fn items_snapshot(&self) -> Vec<TrayItem> {
        self.items.clone()
    }

    fn handle_event(&mut self, event: WatcherEvent, bus: &dyn TrayBus) -> bool {
        match event {
            WatcherEvent::Registered(full_id) => {
                let Some((service, path)) = split_item_id(&full_id) else {
                    return false;
                };
                let Some(props) = bus.fetch_properties(&service, &path) else {
                    return false;
                };
                let item = decode_item(&full_id, &service, &path, &props, &self.icons);
                self.upsert(item);
                true
            }
            WatcherEvent::Unregistered(full_id) => self.remove(&full_id),
            WatcherEvent::RefreshService(sender) => {
                let matching: Vec<(String, String, String)> = self
                    .items
                    .iter()
                    .filter(|item| item.service == sender)
                    .map(|item| {
                        (
                            item.id.clone(),
                            item.service.clone(),
                            item.object_path.clone(),
                        )
                    })
                    .collect();

                let mut changed = false;
                for (full_id, service, path) in matching {
                    if let Some(props) = bus.fetch_properties(&service, &path) {
                        let item = decode_item(&full_id, &service, &path, &props, &self.icons);
                        self.upsert(item);
                        changed = true;
                    }
                }
                changed
            }
            WatcherEvent::OwnerVanished(name) => {
                let vanished: Vec<String> = self
                    .items
                    .iter()
                    .filter(|item| item.service == name)
                    .map(|item| item.id.clone())
                    .collect();

                if vanished.is_empty() {
                    return false;
                }

                for full_id in vanished {
                    if self.remove(&full_id) {
                        bus.emit_unregistered(&full_id);
                    }
                }
                true
            }
        }
    }

    fn upsert(&mut self, item: TrayItem) {
        if let Some(existing) = self.items.iter_mut().find(|old| old.id == item.id) {
            if existing.icon_path != item.icon_path {
                self.icons.remove_temp(&existing.icon_path);
            }
            *existing = item;
            return;
        }

        self.items.push(item);
    }

    fn remove(&mut self, id: &str) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return false;
        };
        let removed = self.items.remove(index);
        self.icons.remove_temp(&removed.icon_path);
        true
    }
}

struct WatcherShared {
    registry: TrayRegistry,
    hosts: Vec<String>,
    tx: mpsc::Sender<WatcherEvent>,
}

struct StatusNotifierWatcher {
    shared: Arc<Mutex<WatcherShared>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.as_str());
        let Some(full_id) = normalize_item_id(service, sender) else {
            return;
        };

        let should_register = {
            let Ok(shared) = self.shared.lock() else {
                return;
            };
            if shared.registry.contains(&full_id) {
                false
            } else {
                let _ = shared.tx.send(WatcherEvent::Registered(full_id.clone()));
                true
            }
        };

        if should_register {
            let _ = Self::status_notifier_item_registered(&emitter, &full_id).await;
        }
    }

    async fn register_status_notifier_host(
        &self,
        service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let inserted = {
            let Ok(mut shared) = self.shared.lock() else {
                return;
            };
            if shared.hosts.iter().any(|h| h == service) {
                false
            } else {
                shared.hosts.push(service.to_string());
                true
            }
        };

        if inserted {
            let _ = Self::status_notifier_host_registered(&emitter).await;
        }
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.shared
            .lock()
            .map(|shared| shared.registry.registered_ids())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

pub fn run() {
    let (tx, rx) = mpsc::channel();
    let host_name = format!("org.kde.StatusNotifierHost-{}", process::id());
    let shared = Arc::new(Mutex::new(WatcherShared {
        registry: TrayRegistry::new(IconStore::from_env()),
        hosts: vec![host_name.clone()],
        tx: tx.clone(),
    }));

    let watcher = StatusNotifierWatcher {
        shared: Arc::clone(&shared),
    };

    let Ok(connection) = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.serve_at(WATCHER_PATH, watcher))
        .and_then(|b| b.build())
    else {
        return;
    };

    let _ = connection.request_name(host_name.as_str());
    let owned_watcher = connection.request_name(WATCHER_NAME).is_ok();

    Tray::write().running = true;

    // If another watcher (e.g. Noctalia) already owns the name, attach as a StatusNotifierHost
    if !owned_watcher {
        attach_to_existing_watcher(&connection, &host_name, &tx);
    }

    // Separate signal-listener connection so incoming signals never contend with method replies
    let signal_tx = tx.clone();
    thread::spawn(move || listen_signals(signal_tx, !owned_watcher));

    let bus = ZbusAdapter {
        connection: &connection,
        owned_watcher,
    };

    for event in rx {
        let updated = {
            let Ok(mut guard) = shared.lock() else {
                continue;
            };
            if guard.registry.handle_event(event, &bus) {
                Some(guard.registry.items_snapshot())
            } else {
                None
            }
        };

        if let Some(items) = updated {
            Tray::write().set_items(items);
        }
    }
}

fn attach_to_existing_watcher(
    connection: &Connection,
    host_name: &str,
    tx: &mpsc::Sender<WatcherEvent>,
) {
    let Ok(proxy) = Proxy::new(connection, WATCHER_NAME, WATCHER_PATH, WATCHER_IFACE) else {
        return;
    };

    let _ = proxy.call::<_, _, ()>("RegisterStatusNotifierHost", &(host_name));

    if let Ok(items) = proxy.get_property::<Vec<String>>("RegisteredStatusNotifierItems") {
        for item in items {
            let _ = tx.send(WatcherEvent::Registered(item));
        }
    }
}

fn listen_signals(tx: mpsc::Sender<WatcherEvent>, watch_external_watcher: bool) {
    let Ok(connection) = Connection::session() else {
        return;
    };

    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&connection) else {
        return;
    };

    let rules = [
        "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
        "type='signal',interface='org.kde.StatusNotifierItem',member='NewIcon'",
        "type='signal',interface='org.kde.StatusNotifierItem',member='NewAttentionIcon'",
        "type='signal',interface='org.kde.StatusNotifierItem',member='NewTitle'",
        "type='signal',interface='org.kde.StatusNotifierItem',member='NewStatus'",
        "type='signal',interface='org.kde.StatusNotifierItem',member='NewToolTip'",
    ];

    for rule in rules {
        if let Ok(parsed) = zbus::MatchRule::try_from(rule) {
            let _ = dbus.add_match_rule(parsed);
        }
    }

    if watch_external_watcher {
        for rule in [
            "type='signal',interface='org.kde.StatusNotifierWatcher',member='StatusNotifierItemRegistered'",
            "type='signal',interface='org.kde.StatusNotifierWatcher',member='StatusNotifierItemUnregistered'",
        ] {
            if let Ok(parsed) = zbus::MatchRule::try_from(rule) {
                let _ = dbus.add_match_rule(parsed);
            }
        }
    }

    for message in MessageIterator::from(&connection).flatten() {
        if message.message_type() != MessageType::Signal {
            continue;
        }

        let header = message.header();
        let iface = header.interface().map(|i| i.as_str()).unwrap_or_default();
        let member = header.member().map(|m| m.as_str()).unwrap_or_default();

        match (iface, member) {
            ("org.freedesktop.DBus", "NameOwnerChanged") => {
                if let Ok((name, _old_owner, new_owner)) =
                    message.body().deserialize::<(String, String, String)>()
                    && new_owner.is_empty()
                {
                    let _ = tx.send(WatcherEvent::OwnerVanished(name));
                }
            }
            (WATCHER_IFACE, "StatusNotifierItemRegistered") => {
                if let Ok(item_id) = message.body().deserialize::<String>() {
                    let _ = tx.send(WatcherEvent::Registered(item_id));
                }
            }
            (WATCHER_IFACE, "StatusNotifierItemUnregistered") => {
                if let Ok(item_id) = message.body().deserialize::<String>() {
                    let _ = tx.send(WatcherEvent::Unregistered(item_id));
                }
            }
            (ITEM_IFACE, _) => {
                if let Some(sender) = header.sender() {
                    let _ = tx.send(WatcherEvent::RefreshService(sender.to_string()));
                }
            }
            _ => {}
        }
    }
}

fn decode_item(
    full_id: &str,
    service: &str,
    path: &str,
    props: &HashMap<String, OwnedValue>,
    icons: &IconStore,
) -> TrayItem {
    let str_prop = |key: &str| -> String {
        props
            .get(key)
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or_default()
            .to_string()
    };

    let item_id = str_prop("Id");
    let title = str_prop("Title");
    let status = {
        let s = str_prop("Status");
        if s.is_empty() {
            String::from("Active")
        } else {
            s
        }
    };

    let icon_name = if status == "NeedsAttention" {
        let attn = str_prop("AttentionIconName");
        if attn.is_empty() {
            str_prop("IconName")
        } else {
            attn
        }
    } else {
        str_prop("IconName")
    };

    let icon_theme_path = str_prop("IconThemePath");
    let pixmaps = extract_pixmaps(props.get("IconPixmap"));
    let icon_path = icons.resolve(&icon_name, &icon_theme_path, &pixmaps);

    let menu_path = props
        .get("Menu")
        .and_then(|v| OwnedObjectPath::try_from(v.try_clone().ok()?).ok())
        .map(|p| p.to_string())
        .filter(|p| !p.is_empty() && p != "/NO_DBUSMENU");

    let item_is_menu = props
        .get("ItemIsMenu")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);

    TrayItem {
        id: full_id.to_string(),
        service: service.to_string(),
        object_path: path.to_string(),
        item_id,
        title,
        icon_name,
        icon_path,
        status,
        menu_path,
        item_is_menu,
    }
}

fn extract_pixmaps(value: Option<&OwnedValue>) -> Vec<(i32, i32, Vec<u8>)> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Ok(cloned) = value.try_clone() else {
        return Vec::new();
    };
    <Vec<(i32, i32, Vec<u8>)>>::try_from(cloned).unwrap_or_default()
}

pub fn call_item_method(service: &str, object_path: &str, method: &str, x: i32, y: i32) {
    let Some(connection) = shared_session() else {
        return;
    };
    let Ok(proxy) = Proxy::new(&connection, service, object_path, ITEM_IFACE) else {
        return;
    };
    let _ = proxy.call::<_, _, ()>(method, &(x, y));
}

pub fn call_item_scroll(service: &str, object_path: &str, delta: i32, horizontal: bool) {
    let Some(connection) = shared_session() else {
        return;
    };
    let Ok(proxy) = Proxy::new(&connection, service, object_path, ITEM_IFACE) else {
        return;
    };
    let orientation = if horizontal { "horizontal" } else { "vertical" };
    let _ = proxy.call::<_, _, ()>("Scroll", &(delta, orientation));
}

/// Normalizes what an app passes to `RegisterStatusNotifierItem` into `"<bus_name><object_path>"`.
pub fn normalize_item_id(service: &str, sender: Option<&str>) -> Option<String> {
    if service.is_empty() {
        return None;
    }

    if service.starts_with('/') {
        let sender = sender?;
        Some(format!("{sender}{service}"))
    } else if service.contains('/') {
        Some(service.to_string())
    } else {
        Some(format!("{service}{DEFAULT_ITEM_PATH}"))
    }
}

/// Splits `"<bus_name><object_path>"` into `(bus_name, object_path)`.
pub fn split_item_id(full_id: &str) -> Option<(String, String)> {
    let slash = full_id.find('/')?;
    let service = &full_id[..slash];
    let path = &full_id[slash..];
    if service.is_empty() || path.is_empty() {
        return None;
    }
    Some((service.to_string(), path.to_string()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use zbus::zvariant::{ObjectPath, Value};

    use super::*;

    static TEST_SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("amane-watcher-test-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[derive(Default)]
    struct FakeTrayBus {
        props: HashMap<(String, String), HashMap<String, OwnedValue>>,
        unregistered: RefCell<Vec<String>>,
    }

    impl FakeTrayBus {
        fn set_item(&mut self, service: &str, path: &str, props: HashMap<String, OwnedValue>) {
            self.props
                .insert((service.to_string(), path.to_string()), props);
        }
    }

    impl TrayBus for FakeTrayBus {
        fn fetch_properties(
            &self,
            service: &str,
            path: &str,
        ) -> Option<HashMap<String, OwnedValue>> {
            let map = self.props.get(&(service.to_string(), path.to_string()))?;
            let mut cloned = HashMap::with_capacity(map.len());
            for (k, v) in map {
                cloned.insert(k.clone(), v.try_clone().ok()?);
            }
            Some(cloned)
        }

        fn emit_unregistered(&self, full_id: &str) {
            self.unregistered.borrow_mut().push(full_id.to_string());
        }
    }

    fn base_props(title: &str, status: &str, pixmap_argb: [u8; 4]) -> HashMap<String, OwnedValue> {
        let mut props = HashMap::new();
        props.insert("Id".into(), Value::from("discord").try_into().unwrap());
        props.insert("Title".into(), Value::from(title).try_into().unwrap());
        props.insert("Status".into(), Value::from(status).try_into().unwrap());
        props.insert(
            "IconPixmap".into(),
            Value::from(vec![(1i32, 1i32, pixmap_argb.to_vec())])
                .try_into()
                .unwrap(),
        );
        props
    }

    #[test]
    fn normalizes_and_splits_sni_item_ids() {
        assert_eq!(
            normalize_item_id("/StatusNotifierItem", Some(":1.42")).as_deref(),
            Some(":1.42/StatusNotifierItem")
        );
        assert_eq!(
            normalize_item_id("org.kde.StatusNotifierItem-123-1", Some(":1.42")).as_deref(),
            Some("org.kde.StatusNotifierItem-123-1/StatusNotifierItem")
        );
        assert_eq!(
            normalize_item_id(":1.99/org/ayatana/NotificationItem/discord", None).as_deref(),
            Some(":1.99/org/ayatana/NotificationItem/discord")
        );

        assert_eq!(
            split_item_id(":1.42/StatusNotifierItem"),
            Some((
                String::from(":1.42"),
                String::from("/StatusNotifierItem")
            ))
        );
        assert_eq!(split_item_id("invalid-no-slash"), None);
    }

    #[test]
    fn registry_lifecycle_cleans_temp_icons_and_emits_unregistered_on_vanish() {
        let dir = temp_dir("lifecycle");
        let mut registry = TrayRegistry::new(IconStore::with_dirs(dir.clone(), Vec::new()));
        let mut bus = FakeTrayBus::default();

        let mut props = base_props("Discord", "Active", [0xff, 0x10, 0x20, 0x30]);
        props.insert(
            "Menu".into(),
            Value::from(ObjectPath::try_from("/NO_DBUSMENU").unwrap())
                .try_into()
                .unwrap(),
        );
        bus.set_item(":1.42", "/StatusNotifierItem", props);

        assert!(registry.handle_event(
            WatcherEvent::Registered(":1.42/StatusNotifierItem".into()),
            &bus
        ));
        assert_eq!(registry.registered_ids(), vec![":1.42/StatusNotifierItem"]);

        let first_icon = registry.items_snapshot()[0]
            .icon_path
            .clone()
            .expect("temp icon saved");
        assert!(Path::new(&first_icon).exists());
        assert!(!registry.items_snapshot()[0].has_menu());

        // Refresh replaces the pixmap and deletes the previous temp PNG
        let mut updated = base_props("Discord (1)", "NeedsAttention", [0xff, 0xaa, 0xbb, 0xcc]);
        updated.insert("IconName".into(), Value::from("fallback-icon").try_into().unwrap());
        updated.insert(
            "AttentionIconName".into(),
            Value::from("").try_into().unwrap(),
        );
        updated.insert(
            "Menu".into(),
            Value::from(ObjectPath::try_from("/MenuBar").unwrap())
                .try_into()
                .unwrap(),
        );
        bus.set_item(":1.42", "/StatusNotifierItem", updated);

        assert!(registry.handle_event(WatcherEvent::RefreshService(":1.42".into()), &bus));
        assert!(!Path::new(&first_icon).exists());

        let item = &registry.items_snapshot()[0];
        assert_eq!(item.title(), "Discord (1)");
        assert_eq!(item.icon_name(), "fallback-icon");
        assert!(item.has_menu());
        let second_icon = item.icon_path.clone().expect("second temp icon saved");
        assert!(Path::new(&second_icon).exists());

        // Owner vanishing removes the item, deletes temp icon, and emits unregistered
        assert!(registry.handle_event(WatcherEvent::OwnerVanished(":1.42".into()), &bus));
        assert!(registry.items_snapshot().is_empty());
        assert!(!Path::new(&second_icon).exists());
        assert_eq!(
            *bus.unregistered.borrow(),
            vec![":1.42/StatusNotifierItem".to_string()]
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
