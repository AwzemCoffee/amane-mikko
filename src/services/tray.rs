mod dbus_menu;
mod icon;
mod watcher;

use zbus::blocking::Connection;

use crate::Service;
use crate::services::worker;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayItem {
    pub(crate) id: String,
    pub(crate) service: String,
    pub(crate) object_path: String,
    pub(crate) item_id: String,
    pub(crate) title: String,
    pub(crate) icon_name: String,
    pub(crate) icon_path: Option<String>,
    pub(crate) status: String,
    pub(crate) menu_path: Option<String>,
    pub(crate) item_is_menu: bool,
}

impl TrayItem {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn icon_name(&self) -> &str {
        &self.icon_name
    }

    pub fn icon_path(&self) -> Option<&str> {
        self.icon_path.as_deref()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn has_menu(&self) -> bool {
        self.menu_path.is_some()
    }

    pub fn item_is_menu(&self) -> bool {
        self.item_is_menu
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub is_separator: bool,
    pub toggle_type: Option<String>,
    pub toggle_state: i32,
    pub children: Vec<TrayMenuItem>,
}

pub struct Tray {
    items: Vec<TrayItem>,
    running: bool,
}

impl Service for Tray {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            running: false,
        }
    }

    fn listen() {
        watcher::run();
    }
}

impl Tray {
    pub fn items(&self) -> &[TrayItem] {
        &self.items
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn activate(id: &str, x: i32, y: i32) {
        let Some((service, path)) = Self::lookup(id) else {
            return;
        };

        worker::run(move || watcher::call_item_method(&service, &path, "Activate", x, y));
    }

    pub fn secondary_activate(id: &str, x: i32, y: i32) {
        let Some((service, path)) = Self::lookup(id) else {
            return;
        };

        worker::run(move || {
            watcher::call_item_method(&service, &path, "SecondaryActivate", x, y);
        });
    }

    pub fn scroll(id: &str, delta: i32, horizontal: bool) {
        let Some((service, path)) = Self::lookup(id) else {
            return;
        };

        worker::run(move || watcher::call_item_scroll(&service, &path, delta, horizontal));
    }

    /// Fetches the current `com.canonical.dbusmenu` layout for a tray item.
    pub fn menu(id: &str) -> Vec<TrayMenuItem> {
        let target = {
            let tray = Self::read();
            tray.items
                .iter()
                .find(|item| item.id == id)
                .and_then(|item| {
                    item.menu_path
                        .as_ref()
                        .map(|menu| (item.service.clone(), menu.clone()))
                })
        };

        let Some((service, menu_path)) = target else {
            return Vec::new();
        };

        let Ok(connection) = Connection::session() else {
            return Vec::new();
        };

        dbus_menu::fetch_menu(&connection, &service, &menu_path)
    }

    pub fn menu_click(id: &str, menu_item_id: i32) {
        let target = {
            let tray = Self::read();
            tray.items
                .iter()
                .find(|item| item.id == id)
                .and_then(|item| {
                    item.menu_path
                        .as_ref()
                        .map(|menu| (item.service.clone(), menu.clone()))
                })
        };

        let Some((service, menu_path)) = target else {
            return;
        };

        worker::run(move || {
            if let Ok(connection) = Connection::session() {
                dbus_menu::send_click(&connection, &service, &menu_path, menu_item_id);
            }
        });
    }

    fn lookup(id: &str) -> Option<(String, String)> {
        let tray = Self::read();
        tray.items
            .iter()
            .find(|item| item.id == id)
            .map(|item| (item.service.clone(), item.object_path.clone()))
    }

    fn upsert(&mut self, item: TrayItem) {
        if let Some(existing) = self.items.iter_mut().find(|old| old.id == item.id) {
            if existing.icon_path != item.icon_path {
                icon::remove_temp(&existing.icon_path);
            }
            *existing = item;
            return;
        }

        self.items.push(item);
    }

    fn remove(&mut self, id: &str) {
        if let Some(index) = self.items.iter().position(|item| item.id == id) {
            let removed = self.items.remove(index);
            icon::remove_temp(&removed.icon_path);
        }
    }
}
