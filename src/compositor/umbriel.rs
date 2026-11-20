use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde_json::Value;

use crate::Workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    raw_id: String,
    selector: String,
    output: Option<String>,
}

// Umbriel workspace IDs are strings like "DP-1:1"; keep a stable 1-based i64 slot for each one
static TARGETS: Mutex<Vec<Target>> = Mutex::new(Vec::new());

pub fn listen(on_change: impl FnMut(Vec<Workspace>)) {
    let Ok(mut child) = Command::new("umbriel")
        .args(["subscribe", "workspaces"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };

    events(BufReader::new(stdout), on_change);

    let _ = child.kill();
    let _ = child.wait();
}

pub fn focus_workspace(id: i64) {
    let Some(target) = target_for(id) else {
        return;
    };

    let args = focus_args(&target);

    let _ = Command::new("umbriel")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn events(reader: impl BufRead, mut on_change: impl FnMut(Vec<Workspace>)) {
    for line in reader.lines() {
        let Ok(line) = line else {
            return;
        };

        if let Some(list) = snapshot(&line) {
            on_change(list);
        }
    }
}

fn snapshot(line: &str) -> Option<Vec<Workspace>> {
    let mut targets = TARGETS.lock().ok()?;

    parse(line, &mut targets)
}

fn target_for(id: i64) -> Option<Target> {
    let slot = usize::try_from(id.checked_sub(1)?).ok()?;
    let targets = TARGETS.lock().ok()?;

    targets.get(slot).cloned()
}

fn focus_args(target: &Target) -> Vec<String> {
    let mut args = vec![
        String::from("msg"),
        String::from("workspace-switch"),
        target.selector.clone(),
    ];

    if let Some(output) = &target.output {
        args.push(String::from("--output"));
        args.push(output.clone());
    }

    args
}

fn register_target(
    targets: &mut Vec<Target>,
    raw_id: &str,
    selector: &str,
    output: Option<&str>,
) -> i64 {
    if let Some(slot) = targets.iter().position(|target| target.raw_id == raw_id) {
        targets[slot].selector = selector.to_string();
        targets[slot].output = output.map(String::from);
        return (slot + 1) as i64;
    }

    targets.push(Target {
        raw_id: raw_id.to_string(),
        selector: selector.to_string(),
        output: output.map(String::from),
    });

    targets.len() as i64
}

fn parse(line: &str, targets: &mut Vec<Target>) -> Option<Vec<Workspace>> {
    let value: Value = serde_json::from_str(line).ok()?;

    if value["event"].as_str() != Some("workspaces") {
        return None;
    }

    let mut list = Vec::new();

    for item in value["data"].as_array()? {
        let raw_id = item["id"].as_str()?;
        let index = u32::try_from(item["index"].as_u64()?).ok()?;
        let named = item["named"].as_bool().unwrap_or(false);
        let raw_name = item["name"].as_str().filter(|name| !name.is_empty());

        let name = if named {
            raw_name.map(String::from)
        } else {
            None
        };

        let selector = name
            .as_deref()
            .map(String::from)
            .unwrap_or_else(|| index.to_string());

        let output = item["output"]
            .as_str()
            .filter(|output| !output.is_empty())
            .map(String::from);

        let id = register_target(targets, raw_id, &selector, output.as_deref());

        let windows = u32::try_from(item["window_count"].as_u64().unwrap_or(0)).unwrap_or(0);

        list.push(Workspace {
            id,
            index,
            name,
            output,
            active: item["active"].as_bool().unwrap_or(false),
            focused: item["focused"].as_bool().unwrap_or(false),
            urgent: item["urgent"].as_bool().unwrap_or(false),
            windows,
        });
    }

    Some(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: &str = r#"{"data":[
        {"active":false,"focused":false,"focused_window":"ebb7","id":"DP-1:1","index":1,"layout":"scrolling","layout_override":false,"name":"1","named":false,"occupied":true,"output":"DP-1","previous":true,"urgent":false,"window_count":2},
        {"active":true,"focused":true,"focused_window":"c787","id":"DP-1:2","index":2,"layout":"scrolling","layout_override":false,"name":"code","named":true,"occupied":true,"output":"DP-1","previous":false,"urgent":true,"window_count":7},
        {"active":false,"focused":false,"focused_window":"","id":"DP-1:3","index":3,"layout":"scrolling","layout_override":false,"name":"3","named":false,"occupied":false,"output":"DP-1","previous":false,"urgent":false,"window_count":0}
    ],"event":"workspaces"}"#;

    #[test]
    fn parses_umbriel_workspace_snapshot() {
        let mut targets = Vec::new();
        let list = parse(SNAPSHOT, &mut targets).expect("valid workspaces event");

        assert_eq!(list.len(), 3);

        assert_eq!(list[0].id, 1);
        assert_eq!(list[0].index, 1);
        assert_eq!(list[0].name, None);
        assert_eq!(list[0].output.as_deref(), Some("DP-1"));
        assert!(!list[0].active);
        assert!(!list[0].focused);
        assert!(!list[0].urgent);
        assert_eq!(list[0].windows, 2);

        assert_eq!(list[1].id, 2);
        assert_eq!(list[1].index, 2);
        assert_eq!(list[1].name.as_deref(), Some("code"));
        assert!(list[1].active);
        assert!(list[1].focused);
        assert!(list[1].urgent);
        assert_eq!(list[1].windows, 7);

        assert_eq!(list[2].id, 3);
        assert_eq!(list[2].windows, 0);
    }

    #[test]
    fn keeps_stable_ids_across_updates() {
        let mut targets = Vec::new();
        let first = parse(SNAPSHOT, &mut targets).unwrap();
        let second = parse(SNAPSHOT, &mut targets).unwrap();

        assert_eq!(first[0].id, second[0].id);
        assert_eq!(first[1].id, second[1].id);
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn builds_workspace_switch_args() {
        let target = Target {
            raw_id: String::from("DP-1:2"),
            selector: String::from("code"),
            output: Some(String::from("DP-1")),
        };

        assert_eq!(
            focus_args(&target),
            vec!["msg", "workspace-switch", "code", "--output", "DP-1"]
        );

        let no_output = Target {
            raw_id: String::from("1"),
            selector: String::from("1"),
            output: None,
        };

        assert_eq!(focus_args(&no_output), vec!["msg", "workspace-switch", "1"]);
    }

    #[test]
    fn ignores_non_workspace_events() {
        let mut targets = Vec::new();
        let line = r#"{"data":[],"event":"windows"}"#;

        assert!(parse(line, &mut targets).is_none());
    }
}
