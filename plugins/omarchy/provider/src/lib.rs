use std::cell::RefCell;
use std::path::Path;

use touchbar_appearance_provider_sdk::bindings::touchbar::plugin::broker::{
    HostEvent, OperationResult,
};
use touchbar_appearance_provider_sdk::{
    AppearanceColor, AppearancePublish, AppearanceScheme, FilesystemReadFile, Guest, RequestId,
    decode_file_chunk, publish, read_file,
};

const SELECTION_MOUNT: &str = "omarchy-runtime";
const SELECTION_PATH: &str = "omarchy-theme-selection";
const CURRENT_MOUNT: &str = "omarchy-current";
const CURRENT_PALETTE: &str = "theme/colors.toml";
const USER_THEMES_MOUNT: &str = "omarchy-user-themes";
const SYSTEM_THEMES_MOUNT: &str = "omarchy-system-themes";
const MAX_SELECTION_BYTES: u64 = 4096;
const MAX_PALETTE_BYTES: u64 = 48 * 1024;

struct OmarchyAppearanceProvider;

#[derive(Clone, Debug)]
enum Pending {
    Idle,
    Selection(RequestId),
    Palette {
        request: RequestId,
        fallback: Option<(String, String)>,
    },
    Publication(RequestId),
}

struct State {
    provider: String,
    pending: Pending,
    last_publication: Option<AppearancePublish>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            provider: String::new(),
            pending: Pending::Idle,
            last_publication: None,
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

impl Guest for OmarchyAppearanceProvider {
    fn start(provider_id: String) -> Result<(), String> {
        if provider_id != "omarchy" {
            return Err("unsupported provider identity".into());
        }
        STATE.with_borrow_mut(|state| {
            state.provider = provider_id;
            begin_selection(state)
        })
    }

    fn tick() -> Result<(), String> {
        STATE.with_borrow_mut(|state| {
            if matches!(state.pending, Pending::Idle) {
                begin_selection(state)?;
            }
            Ok(())
        })
    }

    fn handle_host_event(event: HostEvent) -> Result<(), String> {
        STATE.with_borrow_mut(|state| handle_event(state, event))
    }
}

fn handle_event(state: &mut State, event: HostEvent) -> Result<(), String> {
    let HostEvent::Completion((request_id, result)) = event else {
        return Ok(());
    };
    match state.pending.clone() {
        Pending::Selection(request) if request.into_raw() == request_id => match result {
            OperationResult::Success(payload) => {
                let theme = decode_text(&payload)
                    .ok()
                    .and_then(|selection| selected_theme(&selection));
                if let Some(theme) = theme {
                    begin_palette(
                        state,
                        USER_THEMES_MOUNT,
                        &format!("{theme}/colors.toml"),
                        Some((SYSTEM_THEMES_MOUNT.into(), format!("{theme}/colors.toml"))),
                    )
                } else {
                    begin_current_palette(state)
                }
            }
            OperationResult::Error(_) => begin_current_palette(state),
        },
        Pending::Palette { request, fallback } if request.into_raw() == request_id => {
            match result {
                OperationResult::Success(payload) => {
                    let Some(publication) = decode_text(&payload)
                        .ok()
                        .and_then(|contents| parse_palette(&state.provider, &contents))
                    else {
                        state.pending = Pending::Idle;
                        return Ok(());
                    };
                    if state.last_publication.as_ref() == Some(&publication) {
                        state.pending = Pending::Idle;
                        return Ok(());
                    }
                    let request = publish(&publication)
                        .map_err(|error| format!("publish palette: {error:?}"))?;
                    state.last_publication = Some(publication);
                    state.pending = Pending::Publication(request);
                    Ok(())
                }
                OperationResult::Error(_) => {
                    if let Some((mount, path)) = fallback {
                        begin_palette(state, &mount, &path, None)
                    } else {
                        state.pending = Pending::Idle;
                        Ok(())
                    }
                }
            }
        }
        Pending::Publication(request) if request.into_raw() == request_id => {
            if matches!(result, OperationResult::Error(_)) {
                state.last_publication = None;
            }
            state.pending = Pending::Idle;
            Ok(())
        }
        _ => Ok(()),
    }
}

fn begin_selection(state: &mut State) -> Result<(), String> {
    let request = read_file(&FilesystemReadFile {
        mount: SELECTION_MOUNT.into(),
        path: SELECTION_PATH.into(),
        offset: 0,
        maximum_bytes: MAX_SELECTION_BYTES,
    })
    .map_err(|error| format!("read live theme selection: {error:?}"))?;
    state.pending = Pending::Selection(request);
    Ok(())
}

fn begin_current_palette(state: &mut State) -> Result<(), String> {
    begin_palette(state, CURRENT_MOUNT, CURRENT_PALETTE, None)
}

fn begin_palette(
    state: &mut State,
    mount: &str,
    path: &str,
    fallback: Option<(String, String)>,
) -> Result<(), String> {
    let request = read_file(&FilesystemReadFile {
        mount: mount.into(),
        path: path.into(),
        offset: 0,
        maximum_bytes: MAX_PALETTE_BYTES,
    })
    .map_err(|error| format!("read Omarchy palette: {error:?}"))?;
    state.pending = Pending::Palette { request, fallback };
    Ok(())
}

fn decode_text(payload: &[u8]) -> Result<String, String> {
    let chunk = decode_file_chunk(payload).map_err(|_| "invalid file response")?;
    if chunk.offset != 0 || !chunk.eof {
        return Err("file response was incomplete".into());
    }
    String::from_utf8(chunk.bytes).map_err(|_| "file response was not UTF-8".into())
}

fn selected_theme(selection: &str) -> Option<String> {
    let path = Path::new(selection.trim());
    let name = path.file_stem()?.to_str()?;
    (!name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b' ')))
    .then(|| name.to_owned())
}

fn parse_palette(provider: &str, contents: &str) -> Option<AppearancePublish> {
    let document = contents.parse::<toml::Table>().ok()?;
    let string = |key: &str| document.get(key)?.as_str();
    let color = |key: &str| parse_color(string(key)?);
    let foreground = color("foreground")?;
    let accent = color("accent")?;
    Some(AppearancePublish {
        provider: provider.into(),
        scheme: match string("mode") {
            Some("light") => AppearanceScheme::Light,
            Some("dark") => AppearanceScheme::Dark,
            _ => return None,
        },
        background: color("background")?,
        foreground,
        accent,
        selection: color("selection").unwrap_or(accent),
        muted: color("muted").unwrap_or(foreground),
        destructive: color("red").unwrap_or(AppearanceColor {
            red: 230,
            green: 80,
            blue: 80,
        }),
    })
}

fn parse_color(value: &str) -> Option<AppearanceColor> {
    let value = value.strip_prefix('#')?;
    if value.len() != 6 {
        return None;
    }
    Some(AppearanceColor {
        red: u8::from_str_radix(&value[0..2], 16).ok()?,
        green: u8::from_str_radix(&value[2..4], 16).ok()?,
        blue: u8::from_str_radix(&value[4..6], 16).ok()?,
    })
}

touchbar_appearance_provider_sdk::export!(OmarchyAppearanceProvider);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_omarchy_semantic_colors() {
        let palette = parse_palette(
            "omarchy",
            r##"mode="dark"
background="#101315"
foreground="#cacccc"
accent="#798186"
selection="#343d41"
muted="#4b4e55"
red="#de6145""##,
        )
        .unwrap();
        assert_eq!(palette.provider, "omarchy");
        assert_eq!(palette.background.red, 0x10);
        assert_eq!(palette.destructive.green, 0x61);
    }

    #[test]
    fn accepts_cached_preview_names_without_accepting_paths_as_theme_names() {
        assert_eq!(
            selected_theme("/run/user/1001/omarchy/theme-previews/Tokyo Night.png\n"),
            Some("Tokyo Night".into())
        );
        assert_eq!(selected_theme("/tmp/../bad$name.png"), None);
    }
}
