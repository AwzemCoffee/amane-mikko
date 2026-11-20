use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Structure, Value};

use super::TrayMenuItem;

const INTERFACE: &str = "com.canonical.dbusmenu";

pub fn fetch_menu(
    connection: &Connection,
    service: &str,
    menu_path: &str,
) -> Vec<TrayMenuItem> {
    let proxy = match zbus::blocking::Proxy::new(connection, service, menu_path, INTERFACE) {
        Ok(proxy) => proxy,
        Err(_) => return Vec::new(),
    };

    // Tell the app we are about to show the root menu (some apps lazily populate items here)
    let _ = proxy.call::<_, _, bool>("AboutToShow", &(0i32));

    let properties: Vec<&str> = Vec::new();
    let reply: Result<(u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)), _> =
        proxy.call("GetLayout", &(0i32, -1i32, properties));

    let Ok((_revision, root)) = reply else {
        return Vec::new();
    };

    root.2
        .into_iter()
        .filter_map(parse_node)
        .filter(|item| item.visible)
        .collect()
}

pub fn send_click(connection: &Connection, service: &str, menu_path: &str, item_id: i32) {
    let Ok(proxy) = zbus::blocking::Proxy::new(connection, service, menu_path, INTERFACE) else {
        return;
    };

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    let data = Value::from("");
    let _ = proxy.call::<_, _, ()>("Event", &(item_id, "clicked", data, timestamp));
}

fn parse_node(value: OwnedValue) -> Option<TrayMenuItem> {
    let structure = Structure::try_from(value).ok()?;
    let mut fields = structure.into_fields().into_iter();

    let id = i32::try_from(fields.next()?).ok()?;
    let props_val = fields.next()?;
    let children_val = fields.next()?;

    let props: HashMap<String, OwnedValue> = props_val.try_into().ok().unwrap_or_default();
    let raw_children: Vec<OwnedValue> = children_val.try_into().ok().unwrap_or_default();

    let raw_label = props
        .get("label")
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or_default();

    let label = strip_mnemonic(raw_label);

    let enabled = props
        .get("enabled")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(true);

    let visible = props
        .get("visible")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(true);

    let is_separator = props
        .get("type")
        .and_then(|v| <&str>::try_from(v).ok())
        .is_some_and(|kind| kind == "separator");

    let toggle_type = props
        .get("toggle-type")
        .and_then(|v| <&str>::try_from(v).ok())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let toggle_state = props
        .get("toggle-state")
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(-1);

    let children = raw_children
        .into_iter()
        .filter_map(parse_node)
        .filter(|item| item.visible)
        .collect();

    Some(TrayMenuItem {
        id,
        label,
        enabled,
        visible,
        is_separator,
        toggle_type,
        toggle_state,
        children,
    })
}

/// Strips GTK/Qt `_` keyboard mnemonics (`"_Quit"` -> `"Quit"`, `"A__B"` -> `"A_B"`).
pub fn strip_mnemonic(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
        } else {
            out.push(ch);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::StructureBuilder;

    fn make_node(
        id: i32,
        props: HashMap<String, OwnedValue>,
        children: Vec<OwnedValue>,
    ) -> OwnedValue {
        let structure = StructureBuilder::new()
            .add_field(id)
            .add_field(props)
            .add_field(children)
            .build()
            .unwrap();
        OwnedValue::try_from(Value::Structure(structure)).unwrap()
    }

    #[test]
    fn strips_dbusmenu_mnemonics() {
        assert_eq!(strip_mnemonic("_Quit"), "Quit");
        assert_eq!(strip_mnemonic("Open _Settings"), "Open Settings");
        assert_eq!(strip_mnemonic("Keep__Underscore"), "Keep_Underscore");
    }

    #[test]
    fn parses_dbusmenu_nodes_toggles_separators_and_visibility() {
        let mut child_hidden_props = HashMap::new();
        child_hidden_props.insert("label".into(), Value::from("Hidden").try_into().unwrap());
        child_hidden_props.insert("visible".into(), Value::from(false).try_into().unwrap());
        let hidden_child = make_node(2, child_hidden_props, Vec::new());

        let mut sep_props = HashMap::new();
        sep_props.insert("type".into(), Value::from("separator").try_into().unwrap());
        let sep_child = make_node(3, sep_props, Vec::new());

        let mut root_props = HashMap::new();
        root_props.insert("label".into(), Value::from("_Mute").try_into().unwrap());
        root_props.insert("enabled".into(), Value::from(true).try_into().unwrap());
        root_props.insert(
            "toggle-type".into(),
            Value::from("checkmark").try_into().unwrap(),
        );
        root_props.insert("toggle-state".into(), Value::from(1i32).try_into().unwrap());

        let parsed = parse_node(make_node(1, root_props, vec![hidden_child, sep_child]))
            .expect("should parse node");

        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.label, "Mute");
        assert!(parsed.enabled);
        assert!(parsed.visible);
        assert!(!parsed.is_separator);
        assert_eq!(parsed.toggle_type.as_deref(), Some("checkmark"));
        assert_eq!(parsed.toggle_state, 1);
        // Hidden child filtered out, separator kept
        assert_eq!(parsed.children.len(), 1);
        assert_eq!(parsed.children[0].id, 3);
        assert!(parsed.children[0].is_separator);
    }
}
