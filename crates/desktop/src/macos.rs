use std::{
    env,
    error::Error,
    process::{Command, Stdio},
};

use muda::{
    AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem as SystemItem, Submenu,
    accelerator::{Accelerator, CMD_OR_CTRL, Code},
};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::macos::WindowBuilderExtMacOS,
    window::{UserAttentionType, WindowBuilder},
};
use wry::{NewWindowResponse, WebView, WebViewBuilder};

use crate::{
    daemon,
    links::{Navigation, app_url, http_url, navigation, open_external},
};

const TITLE: &str = "T3 Code RS";
const DEFAULT_URL: &str = "http://127.0.0.1:3774";
const BACKGROUND: (u8, u8, u8, u8) = (10, 10, 10, 255);
const NEW_THREAD: &str = "new-thread";
const RELOAD: &str = "reload";
const TOGGLE_SIDEBAR: &str = "toggle-sidebar";
const SEARCH: &str = "search";
const NOTIFY_PREFIX: &str = "notify:";
const NOTIFICATION_TITLE_MAX: usize = 120;
const NOTIFICATION_BODY_MAX: usize = 1000;
// Arguments arrive as argv, so the script never interpolates renderer-supplied text.
const NOTIFICATION_SCRIPT: &str = "on run argv
  display notification (item 2 of argv) with title (item 1 of argv)
end run";

const INITIALIZATION: &str = r#"
window.__T3_DESKTOP__ = true;
document.addEventListener('DOMContentLoaded', () => {
  document.documentElement.classList.add('t3-desktop');
});
if (document.documentElement) document.documentElement.classList.add('t3-desktop');
"#;

enum AppEvent {
    Menu(MenuEvent),
    Drag,
    ToggleMaximize,
    Navigate(String),
    Notify(Notification),
}

#[derive(Debug, PartialEq, Eq)]
struct Notification {
    title: String,
    body: String,
}

/// Parses the JSON payload of a `notify:` message. Unknown fields such as `thread_id` are ignored.
fn parse_notification(raw: &str) -> Option<Notification> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    };
    let title = clean_text(field("title"), NOTIFICATION_TITLE_MAX);
    let body = clean_text(field("body"), NOTIFICATION_BODY_MAX);
    if title.is_empty() && body.is_empty() {
        return None;
    }
    Some(Notification {
        title: if title.is_empty() {
            TITLE.into()
        } else {
            title
        },
        body,
    })
}

/// Caps length in characters and turns control characters (newlines, tabs) into spaces.
fn clean_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .take(max_chars)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Shows a notification via osascript. `--` ends osascript's options so title and body are argv only.
fn show_notification(notification: Notification) {
    std::thread::spawn(move || {
        let result = Command::new("/usr/bin/osascript")
            .args(["-e", NOTIFICATION_SCRIPT, "--"])
            .arg(&notification.title)
            .arg(&notification.body)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(result, Ok(status) if status.success()) {
            eprintln!("T3 Code RS: could not show the desktop notification");
        }
    });
}

pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let raw_url = env::var("T3_APP_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let url = app_url(&raw_url)
        .ok_or("T3_APP_URL must use https://, http://127.0.0.1, or http://localhost")?;
    let origin = url.origin();
    let daemon = if url.scheme() == "http" && url.port() == Some(daemon::PORT) {
        daemon::ensure_running(url.host_str() == Some("localhost"))?
    } else {
        None
    };

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(1280, 860))
        .with_min_inner_size(LogicalSize::new(840, 560))
        .with_titlebar_transparent(true)
        .with_fullsize_content_view(true)
        .with_title_hidden(true)
        .with_background_color(BACKGROUND)
        .with_visible(false)
        .build(&event_loop)?;

    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(AppEvent::Menu(event));
    }));
    let menu = install_menu()?;

    let ipc_proxy = event_loop.create_proxy();
    let navigation_origin = origin.clone();
    let popup_origin = origin.clone();
    let popup_proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new()
        .with_url(url.as_str())
        .with_background_color(BACKGROUND)
        .with_initialization_script(INITIALIZATION)
        // WKWebView provides clipboard support; AppKit routes editing shortcuts via the Edit menu.
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |request| {
            let uri = request.uri().to_string();
            if !http_url(&uri).is_some_and(|sender| sender.origin() == origin) {
                return;
            }
            let event = match request.body().as_str() {
                "drag" => AppEvent::Drag,
                "toggle-maximize" => AppEvent::ToggleMaximize,
                message => {
                    if let Some(url) = message.strip_prefix("open-external:").and_then(http_url) {
                        open_external(url);
                    } else if let Some(notification) = message
                        .strip_prefix(NOTIFY_PREFIX)
                        .and_then(parse_notification)
                    {
                        let _ = ipc_proxy.send_event(AppEvent::Notify(notification));
                    }
                    return;
                }
            };
            let _ = ipc_proxy.send_event(event);
        })
        .with_navigation_handler(
            move |target| match navigation(&target, &navigation_origin) {
                Navigation::Allow => true,
                Navigation::External(url) => {
                    open_external(url);
                    false
                }
                Navigation::Block => false,
            },
        )
        .with_new_window_req_handler(move |target, _| {
            match navigation(&target, &popup_origin) {
                Navigation::Allow if target != "about:blank" => {
                    let _ = popup_proxy.send_event(AppEvent::Navigate(target));
                }
                Navigation::External(url) => open_external(url),
                _ => {}
            }
            NewWindowResponse::Deny
        })
        .build(&window)?;
    window.set_visible(true);

    // Tao exits the process after LoopDestroyed, so explicitly drop the view and daemon here.
    let mut session = Some((webview, menu, daemon));
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::LoopDestroyed => drop(session.take()),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *flow = ControlFlow::Exit,
            Event::Reopen {
                has_visible_windows: false,
                ..
            } => {
                window.set_minimized(false);
                window.set_visible(true);
                window.set_focus();
            }
            Event::UserEvent(event) => {
                let Some((webview, _, _)) = &session else {
                    return;
                };
                match event {
                    AppEvent::Menu(event) => handle_menu(event, webview),
                    AppEvent::Drag => {
                        let _ = window.drag_window();
                    }
                    AppEvent::ToggleMaximize => window.set_maximized(!window.is_maximized()),
                    AppEvent::Navigate(url) => {
                        let _ = webview.load_url(&url);
                    }
                    AppEvent::Notify(notification) => {
                        if !window.is_focused() {
                            show_notification(notification);
                            window.request_user_attention(Some(UserAttentionType::Informational));
                        }
                    }
                }
            }
            _ => {}
        }
    })
}

fn install_menu() -> muda::Result<Menu> {
    let command = |id, title, key| {
        MenuItem::with_id(id, title, true, Some(Accelerator::new(CMD_OR_CTRL, key)))
    };
    let about = AboutMetadata {
        name: Some(TITLE.into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        ..Default::default()
    };
    let app = Submenu::with_items(
        TITLE,
        true,
        &[
            &SystemItem::about(Some("About T3 Code RS"), Some(about)),
            &SystemItem::separator(),
            &SystemItem::hide(Some("Hide T3 Code RS")),
            &SystemItem::hide_others(None),
            &SystemItem::show_all(None),
            &SystemItem::separator(),
            &SystemItem::quit(Some("Quit T3 Code RS")),
        ],
    )?;
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &command(NEW_THREAD, "New Thread", Code::KeyN),
            &SystemItem::close_window(Some("Close Window")),
        ],
    )?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &SystemItem::undo(None),
            &SystemItem::redo(None),
            &SystemItem::separator(),
            &SystemItem::cut(None),
            &SystemItem::copy(None),
            &SystemItem::paste(None),
            &SystemItem::select_all(None),
        ],
    )?;
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &command(RELOAD, "Reload", Code::KeyR),
            &command(TOGGLE_SIDEBAR, "Toggle Sidebar", Code::KeyB),
            &command(SEARCH, "Search", Code::KeyK),
        ],
    )?;
    let window = Submenu::with_items(
        "Window",
        true,
        &[&SystemItem::minimize(None), &SystemItem::maximize(None)],
    )?;
    let menu = Menu::with_items(&[&app, &file, &edit, &view, &window])?;
    menu.init_for_nsapp();
    window.set_as_windows_menu_for_nsapp();
    Ok(menu)
}

fn handle_menu(event: MenuEvent, webview: &WebView) {
    let (key, fallback) = match event.id().as_ref() {
        NEW_THREAD => ("n", "document.getElementById('btn-new-thread')?.click()"),
        TOGGLE_SIDEBAR => (
            "b",
            "Array.from(document.querySelectorAll('#btn-sb-open, #btn-sb-collapse, #btn-menu'))
                .find(button => button.getClientRects().length)?.click()",
        ),
        SEARCH => ("k", "document.getElementById('btn-search')?.click()"),
        RELOAD => {
            let _ = webview.reload();
            return;
        }
        _ => return,
    };
    let script = format!(
        "if (document.dispatchEvent(new KeyboardEvent('keydown', {{
            key: '{key}', metaKey: true, bubbles: true, cancelable: true
        }})) && !document.querySelector('dialog[open]')) {{ {fallback}; }}"
    );
    let _ = webview.evaluate_script(&script);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_payload_is_parsed_and_unknown_fields_ignored() {
        let parsed =
            parse_notification(r#"{"title":"Done","body":"Run finished","thread_id":"t1"}"#);
        assert_eq!(
            parsed,
            Some(Notification {
                title: "Done".into(),
                body: "Run finished".into(),
            })
        );
    }

    #[test]
    fn invalid_or_empty_notification_payloads_are_ignored() {
        for raw in [
            "",
            "not json",
            "[]",
            r#"{"title":42}"#,
            r#"{"title":"  ","body":""}"#,
        ] {
            assert_eq!(parse_notification(raw), None, "{raw}");
        }
    }

    #[test]
    fn missing_title_falls_back_to_app_name() {
        let parsed = parse_notification(r#"{"body":"Only a body"}"#).unwrap();
        assert_eq!(parsed.title, TITLE);
        assert_eq!(parsed.body, "Only a body");
    }

    #[test]
    fn notification_text_is_capped_and_control_characters_replaced() {
        let long_title = "あ".repeat(NOTIFICATION_TITLE_MAX + 5);
        let long_body = "b".repeat(NOTIFICATION_BODY_MAX + 5);
        let payload = serde_json::json!({
            "title": long_title,
            "body": format!("line1\nline2\t{long_body}"),
        });
        let parsed = parse_notification(&payload.to_string()).unwrap();
        assert_eq!(parsed.title.chars().count(), NOTIFICATION_TITLE_MAX);
        assert_eq!(parsed.body.chars().count(), NOTIFICATION_BODY_MAX);
        assert!(parsed.body.starts_with("line1 line2 "));
        assert!(!parsed.body.contains('\n') && !parsed.body.contains('\t'));
    }

    #[test]
    fn shell_metacharacters_stay_plain_argv_text() {
        let raw = r#"{"title":"$(rm -rf ~)","body":"\"; do shell; `id`"}"#;
        let parsed = parse_notification(raw).unwrap();
        assert_eq!(parsed.title, "$(rm -rf ~)");
        assert_eq!(parsed.body, "\"; do shell; `id`");
    }
}
