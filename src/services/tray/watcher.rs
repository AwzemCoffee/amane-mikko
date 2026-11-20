use std::collections::HashMap;
use std::process;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::{Header, Type as MessageType};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::{Tray, TrayItem, icon};
use crate::Service;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";

#[derive(Debug)]
enum WatcherEvent {
    Registered(String),
    Unregistered(String),
    RefreshService(String),
    OwnerVanished(String),
}

struct WatcherState {
    items: Vec<String>,
    hosts: Vec<String>,
    tx: mpsc::Sender<WatcherEvent>,
}

struct StatusNotifierWatcher {
    state: Arc<Mutex<WatcherState>>,
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

        let inserted = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.items.contains(&full_id) {
                false
            } else {
                state.items.push(full_id.clone());
                let _ = state.tx.send(WatcherEvent::Registered(full_id.clone()));
                true
            }
        };

        if inserted {
            let _ = Self::status_notifier_item_registered(&emitter, &full_id).await;
        }
    }

    async fn register_status_notifier_host(
        &self,
        service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let inserted = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.hosts.iter().any(|h| h == service) {
                false
            } else {
                state.hosts.push(service.to_string());
                true
            }
        };

        if inserted {
            let _ = Self::status_notifier_host_registered(&emitter).await;
        }
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.items.clone())
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
    let state = Arc::new(Mutex::new(WatcherState {
        items: Vec::new(),
        hosts: vec![format!("org.kde.StatusNotifierHost-{}", process::id())],
        tx: tx.clone(),
    }));

    let watcher = StatusNotifierWatcher {
        state: Arc::clone(&state),
    };

    let Ok(connection) = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.serve_at(WATCHER_PATH, watcher))
        .and_then(|b| b.build())
    else {
        return;
    };

    let host_name = format!("org.kde.StatusNotifierHost-{}", process::id());
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

    // Event processing loop: fetch item properties and update Tray service reactively
    let mut active_items: HashMap<String, (String, String)> = HashMap::new();

    for event in rx {
        match event {
            WatcherEvent::Registered(full_id) => {
                let Some((service, path)) = split_item_id(&full_id) else {
                    continue;
                };
                active_items.insert(full_id.clone(), (service.clone(), path.clone()));
                if let Some(item) = fetch_item(&connection, &full_id, &service, &path) {
                    Tray::write().upsert(item);
                }
            }
            WatcherEvent::Unregistered(full_id) => {
                active_items.remove(&full_id);
                if owned_watcher
                    && let Ok(mut guard) = state.lock()
                {
                    guard.items.retain(|id| id != &full_id);
                }
                Tray::write().remove(&full_id);
            }
            WatcherEvent::RefreshService(sender) => {
                let matching: Vec<(String, String, String)> = active_items
                    .iter()
                    .filter(|(_, (svc, _))| svc == &sender)
                    .map(|(id, (svc, path))| (id.clone(), svc.clone(), path.clone()))
                    .collect();

                for (full_id, service, path) in matching {
                    if let Some(item) = fetch_item(&connection, &full_id, &service, &path) {
                        Tray::write().upsert(item);
                    }
                }
            }
            WatcherEvent::OwnerVanished(name) => {
                let vanished: Vec<String> = active_items
                    .iter()
                    .filter(|(_, (svc, _))| svc == &name)
                    .map(|(id, _)| id.clone())
                    .collect();

                for full_id in vanished {
                    active_items.remove(&full_id);
                    if owned_watcher
                        && let Ok(mut guard) = state.lock()
                    {
                        guard.items.retain(|id| id != &full_id);
                    }
                    Tray::write().remove(&full_id);
                }
            }
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

fn fetch_item(
    connection: &Connection,
    full_id: &str,
    service: &str,
    path: &str,
) -> Option<TrayItem> {
    let proxy = Proxy::new(connection, service, path, "org.freedesktop.DBus.Properties").ok()?;
    let props: HashMap<String, OwnedValue> = proxy.call("GetAll", &(ITEM_IFACE)).ok()?;

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
    let icon_path = icon::resolve(&icon_name, &icon_theme_path, &pixmaps);

    let menu_path = props
        .get("Menu")
        .and_then(|v| OwnedObjectPath::try_from(v.try_clone().ok()?).ok())
        .map(|p| p.to_string())
        .filter(|p| !p.is_empty() && p != "/NO_DBUSMENU");

    let item_is_menu = props
        .get("ItemIsMenu")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);

    Some(TrayItem {
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
    })
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
    let Ok(connection) = Connection::session() else {
        return;
    };
    let Ok(proxy) = Proxy::new(&connection, service, object_path, ITEM_IFACE) else {
        return;
    };
    let _ = proxy.call::<_, _, ()>(method, &(x, y));
}

pub fn call_item_scroll(service: &str, object_path: &str, delta: i32, horizontal: bool) {
    let Ok(connection) = Connection::session() else {
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
    use super::*;

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
}
