use std::sync::atomic::{AtomicU32, Ordering};

use touchbar_component_sdk::kit::{
    ColorRole, InputEvent, InputKind, Pressable, ViewBuilder, content, flexible,
};
use touchbar_component_sdk::{
    CommandRunRequest, CommandValue, Guest, HostEvent, Item, PresentationEvent, RenderRequest,
    Update, View, command_run,
};

struct Hyprland;
static WORKSPACE: AtomicU32 = AtomicU32::new(1);
static DESTINATION: AtomicU32 = AtomicU32::new(2);

impl Guest for Hyprland {
    fn items() -> Vec<Item> {
        vec![
            item("workspaces", "Workspaces"),
            item("window-actions", "Window Actions"),
            item("move-window", "Move Window"),
        ]
    }
    fn render(request: RenderRequest) -> Result<View, String> {
        Ok(match request.item_id.as_str() {
            "workspaces" => workspace_view(request.viewport.width),
            "window-actions" => actions_view(),
            "move-window" => move_view(),
            item => return Err(format!("unknown item {item}")),
        })
    }
    fn handle_event(event: InputEvent) -> Result<Update, String> {
        match (event.widget_id, event.kind) {
            (10, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    let workspace = from_slider(value);
                    WORKSPACE.store(workspace, Ordering::Relaxed);
                    run_workspace("workspace", workspace);
                }
            }
            (20, InputKind::Activated) => run("fullscreen", Vec::new()),
            (21, InputKind::Activated) => run("toggle-float", Vec::new()),
            (30, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    DESTINATION.store(from_slider(value), Ordering::Relaxed);
                }
            }
            (31, InputKind::Activated) => {
                run_workspace("move-window", DESTINATION.load(Ordering::Relaxed))
            }
            _ => {}
        }
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }
    fn handle_host_event(_event: HostEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}
fn from_slider(value: f32) -> u32 {
    1 + (value.clamp(0.0, 1.0) * 9.0).round() as u32
}
fn run_workspace(command: &str, workspace: u32) {
    run(
        command,
        vec![CommandValue::Integer {
            name: "workspace".into(),
            value: i64::from(workspace),
        }],
    );
}
fn run(command_id: &str, values: Vec<CommandValue>) {
    let _ = command_run(&CommandRunRequest {
        command_id: command_id.into(),
        values,
    });
}

fn workspace_view(width: f32) -> View {
    let workspace = WORKSPACE.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let current = b.label(format!("WS {workspace}"), 12.0, ColorRole::OnAccent);
    let current = b.pressable(11, "Current workspace", current, Pressable::accent());
    if width < 120.0 {
        return b.finish(current);
    }
    let slider = b.slider(10, "Workspace 1 through 10", (workspace - 1) as f32 / 9.0);
    let root = b.row(
        vec![
            content(current, 44.0, 64.0),
            flexible(slider, 60.0, 180.0, 900.0),
        ],
        5.0,
        3.0,
    );
    b.finish(root)
}
fn actions_view() -> View {
    let mut b = ViewBuilder::new();
    let fullscreen = b.button(20, "FULL", None, false, false);
    let float = b.button(21, "FLOAT", None, false, false);
    let root = b.row(
        vec![
            flexible(fullscreen, 44.0, 80.0, 400.0),
            flexible(float, 44.0, 80.0, 400.0),
        ],
        4.0,
        2.0,
    );
    b.finish(root)
}
fn move_view() -> View {
    let destination = DESTINATION.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let label = b.label(format!("MOVE → {destination}"), 11.0, ColorRole::OnAccent);
    let commit = b.pressable(31, "Move focused window", label, Pressable::accent());
    let slider = b.slider(30, "Destination workspace", (destination - 1) as f32 / 9.0);
    let root = b.row(
        vec![
            content(commit, 64.0, 100.0),
            flexible(slider, 50.0, 140.0, 800.0),
        ],
        5.0,
        3.0,
    );
    b.finish(root)
}

touchbar_component_sdk::export!(Hyprland);
