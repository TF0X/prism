#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{ResizeDirection, WindowBuilder},
};
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    http::{Request, Response},
};

const CHROME_HTML: &str = include_str!("chrome.html");
const GLASS_JS: &str = include_str!("glass.js");

/// Chrome sits along the bottom. It cannot sit on top: a non-child webview is
/// force-resized to the full window on every WM_SIZE by wry's subclass proc,
/// so it can never be inset below a top bar -- and only a *non-child* webview
/// composites transparently with the DWM acrylic backdrop. Bottom also means
/// sites' sticky top headers keep working.
const CHROME_H: u32 = 76;

const HOME: &str = "https://www.youtube.com";
/// Freeze the renderer once the window has been minimized this long.
const IDLE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(15);

enum Ev {
    Diag(String),
    ChromeReady,
    PageReady,
    NewTab(Option<String>),
    CloseTab(u32),
    SelectTab(u32),
    Navigate(String),
    Back,
    Forward,
    Reload,
    GlassCycle,
    Title(String),
    Load(bool, String),
    FocusUrl,
    Drag,
    Resize(ResizeDirection),
    Minimize,
    Close,
}

struct Tab {
    id: u32,
    url: String,
    title: String,
}

fn state_dir() -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("prism");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn load_glass() -> u8 {
    std::fs::read_to_string(state_dir().join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["glass"].as_u64())
        .unwrap_or(1)
        .min(2) as u8
}

fn save_glass(level: u8) {
    let _ = std::fs::write(
        state_dir().join("state.json"),
        serde_json::json!({ "glass": level }).to_string(),
    );
}

fn diag(msg: &str) {
    if std::env::var_os("PRISM_DIAG").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("diag.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Turns address-bar text into a URL; anything not plausibly a host searches.
fn to_url(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return HOME.to_owned();
    }
    if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("file://") {
        return t.to_owned();
    }
    let first = t.split('/').next().unwrap_or(t);
    let host_like =
        !t.contains(' ') && first.contains('.') && !first.starts_with('.') && !first.ends_with('.');
    if host_like || t.starts_with("localhost") {
        format!("https://{t}")
    } else {
        format!(
            "https://duckduckgo.com/?q={}",
            t.replace('&', "%26").replace(' ', "+").replace('#', "%23")
        )
    }
}

fn serve(req: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let body: &'static [u8] = match req.uri().path() {
        "/" | "/index.html" => CHROME_HTML.as_bytes(),
        _ => b"not found",
    };
    Response::builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Cow::Borrowed(body))
        .unwrap()
}

fn chrome_rect(size: PhysicalSize<u32>) -> Rect {
    Rect {
        position: PhysicalPosition::new(0, size.height.saturating_sub(CHROME_H) as i32).into(),
        size: PhysicalSize::new(size.width.max(1), CHROME_H).into(),
    }
}

/// `trusted` is false for messages arriving from a web page. Pages get the
/// window-manipulation verbs only; anything that could drive the browser
/// itself (navigate, open or close tabs) is ignored, so a hostile site cannot
/// steer the chrome it shares a process with.
fn parse(body: &str, trusted: bool) -> Option<Ev> {
    let m: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = || m["id"].as_u64().unwrap_or_default() as u32;
    let action = m["action"].as_str()?;

    let ev = match action {
        "diag" => Ev::Diag(m["msg"].as_str().unwrap_or_default().to_owned()),
        "drag" => Ev::Drag,
        "minimize" => Ev::Minimize,
        "close" if trusted => Ev::Close,
        "resize" => Ev::Resize(match m["dir"].as_str()? {
            "n" => ResizeDirection::North,
            "s" => ResizeDirection::South,
            "w" => ResizeDirection::West,
            "e" => ResizeDirection::East,
            "nw" => ResizeDirection::NorthWest,
            "ne" => ResizeDirection::NorthEast,
            "sw" => ResizeDirection::SouthWest,
            _ => ResizeDirection::SouthEast,
        }),
        "ready" => {
            if trusted {
                Ev::ChromeReady
            } else {
                Ev::PageReady
            }
        }
        _ if !trusted => return None,
        "navigate" => Ev::Navigate(m["text"].as_str().unwrap_or_default().to_owned()),
        "newtab" => Ev::NewTab(None),
        "closetab" => Ev::CloseTab(id()),
        "selecttab" => Ev::SelectTab(id()),
        "back" => Ev::Back,
        "forward" => Ev::Forward,
        "reload" => Ev::Reload,
        "glasscycle" => Ev::GlassCycle,
        "focusurl" => Ev::FocusUrl,
        _ => return None,
    };
    Some(ev)
}

fn main() -> wry::Result<()> {
    let event_loop = EventLoopBuilder::<Ev>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let mut builder = WindowBuilder::new()
        .with_title("Prism")
        .with_inner_size(LogicalSize::new(1100.0, 760.0))
        .with_min_inner_size(LogicalSize::new(420.0, 320.0))
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true);

    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder = builder.with_undecorated_shadow(false);
    }

    let window = builder.build(&event_loop).unwrap();

    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowExtWindows;
        window.set_undecorated_shadow(true);
        let _ = window_vibrancy::apply_acrylic(&window, None);
    }

    let ctx: &'static mut WebContext =
        Box::leak(Box::new(WebContext::new(Some(state_dir().join("profile")))));

    let (p_ipc, p_title, p_load, p_new) =
        (proxy.clone(), proxy.clone(), proxy.clone(), proxy.clone());

    // Content: attached to the window itself, never a child. This is the only
    // arrangement where transparency reaches the acrylic backdrop.
    let mut cb = WebViewBuilder::new_with_web_context(ctx)
        .with_url(HOME)
        .with_transparent(true)
        .with_initialization_script(GLASS_JS)
        .with_ipc_handler(move |r: Request<String>| {
            if let Some(ev) = parse(r.body(), false) {
                let _ = p_ipc.send_event(ev);
            }
        })
        .with_document_title_changed_handler(move |t| {
            let _ = p_title.send_event(Ev::Title(t));
        })
        .with_on_page_load_handler(move |ev, url| {
            let _ = p_load.send_event(Ev::Load(matches!(ev, PageLoadEvent::Finished), url));
        })
        .with_new_window_req_handler(move |url, _| {
            let _ = p_new.send_event(Ev::NewTab(Some(url)));
            NewWindowResponse::Deny
        });

    #[cfg(target_os = "windows")]
    {
        use wry::WebViewBuilderExtWindows;
        cb = cb.with_browser_accelerator_keys(false);
    }

    let content = cb.build(&window)?;

    // Chrome: a child webview along the bottom. Child HWNDs cannot composite
    // transparently, but keeping it on its own origin is what stops a page
    // from reading or spoofing the address bar.
    let p_chrome = proxy.clone();
    let chrome = WebViewBuilder::new()
        .with_bounds(chrome_rect(window.inner_size()))
        .with_transparent(true)
        .with_custom_protocol("prism".into(), |_id, req| serve(&req))
        .with_url("prism://localhost/index.html")
        .with_ipc_handler(move |r: Request<String>| {
            if let Some(ev) = parse(r.body(), true) {
                let _ = p_chrome.send_event(ev);
            }
        })
        .build_as_child(&window)?;

    let mut tabs: Vec<Tab> = vec![Tab {
        id: 1,
        url: HOME.to_owned(),
        title: String::new(),
    }];
    let mut active: u32 = 1;
    let mut next_id: u32 = 2;
    let mut glass = load_glass();
    let mut loading = true;
    let mut idle_since: Option<Instant> = None;
    let mut frozen = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);

        macro_rules! sync_tabs {
            () => {{
                let list: Vec<serde_json::Value> = tabs
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "title": t.title,
                            "active": t.id == active,
                            "loading": t.id == active && loading,
                        })
                    })
                    .collect();
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.tabs({})",
                    serde_json::Value::Array(list)
                ));
            }};
        }

        macro_rules! sync_nav {
            () => {{
                let url = tabs
                    .iter()
                    .find(|t| t.id == active)
                    .map(|t| t.url.clone())
                    .unwrap_or_default();
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.nav({})",
                    serde_json::json!({
                        "url": url,
                        "secure": url.starts_with("https://"),
                        "canBack": content.can_go_back().unwrap_or(false),
                        "canFwd": content.can_go_forward().unwrap_or(false),
                    })
                ));
            }};
        }

        macro_rules! sync_glass {
            () => {{
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.glass({},'{}')",
                    glass,
                    ["Off", "Frosted", "Clear"][glass as usize]
                ));
            }};
        }

        /// Loads whichever tab is active. Tabs share one webview, so switching
        /// is a navigation -- live page state does not survive it.
        macro_rules! open_active {
            () => {{
                if let Some(t) = tabs.iter().find(|t| t.id == active) {
                    loading = true;
                    let _ = content.load_url(&t.url);
                }
            }};
        }

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                let minimized = window.is_minimized();
                if frozen && !minimized {
                    resume(&content);
                    frozen = false;
                } else if !frozen
                    && minimized
                    && idle_since.is_some_and(|t| t.elapsed() >= IDLE)
                {
                    frozen = suspend(&content);
                }
            }

            Event::UserEvent(Ev::Diag(msg)) => diag(&msg),

            Event::UserEvent(Ev::ChromeReady) => {
                sync_tabs!();
                sync_nav!();
                sync_glass!();
            }

            Event::UserEvent(Ev::PageReady) => {
                let _ = content.evaluate_script(&format!(
                    "window.__prismGlass&&window.__prismGlass({glass},{CHROME_H})"
                ));
            }

            Event::UserEvent(Ev::NewTab(url)) => {
                let id = next_id;
                next_id += 1;
                tabs.push(Tab {
                    id,
                    url: url.unwrap_or_else(|| HOME.to_owned()),
                    title: String::new(),
                });
                active = id;
                open_active!();
                sync_tabs!();
                sync_nav!();
            }

            Event::UserEvent(Ev::CloseTab(id)) => {
                if let Some(i) = tabs.iter().position(|t| t.id == id) {
                    tabs.remove(i);
                    if tabs.is_empty() {
                        tabs.push(Tab {
                            id: next_id,
                            url: HOME.to_owned(),
                            title: String::new(),
                        });
                        active = next_id;
                        next_id += 1;
                        open_active!();
                    } else if active == id {
                        active = tabs[i.min(tabs.len() - 1)].id;
                        open_active!();
                    }
                    sync_tabs!();
                    sync_nav!();
                }
            }

            Event::UserEvent(Ev::SelectTab(id)) => {
                if id != active && tabs.iter().any(|t| t.id == id) {
                    active = id;
                    open_active!();
                    sync_tabs!();
                    sync_nav!();
                }
            }

            Event::UserEvent(Ev::Navigate(text)) => {
                let url = to_url(&text);
                if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                    t.url = url.clone();
                }
                loading = true;
                let _ = content.load_url(&url);
                sync_tabs!();
            }

            Event::UserEvent(Ev::Back) => {
                let _ = content.go_back();
            }
            Event::UserEvent(Ev::Forward) => {
                let _ = content.go_forward();
            }
            Event::UserEvent(Ev::Reload) => {
                let _ = content.reload();
            }

            Event::UserEvent(Ev::GlassCycle) => {
                glass = (glass + 1) % 3;
                save_glass(glass);
                let _ = content.evaluate_script(&format!(
                    "window.__prismGlass&&window.__prismGlass({glass},{CHROME_H})"
                ));
                sync_glass!();
            }

            Event::UserEvent(Ev::Title(title)) => {
                if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                    t.title = title;
                }
                sync_tabs!();
            }

            Event::UserEvent(Ev::Load(finished, url)) => {
                if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                    t.url = url;
                }
                loading = !finished;
                sync_tabs!();
                sync_nav!();
            }

            Event::UserEvent(Ev::FocusUrl) => {
                let _ = chrome.evaluate_script("window.__prism&&window.__prism.focusUrl()");
            }

            Event::UserEvent(Ev::Drag) => {
                let _ = window.drag_window();
            }
            Event::UserEvent(Ev::Resize(dir)) => {
                let _ = window.drag_resize_window(dir);
            }
            Event::UserEvent(Ev::Minimize) => window.set_minimized(true),

            Event::WindowEvent {
                event: WindowEvent::Focused(has_focus),
                ..
            } => {
                idle_since = if has_focus { None } else { Some(Instant::now()) };
                if has_focus && frozen {
                    resume(&content);
                    frozen = false;
                }
            }

            Event::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                if size.width > 0 && size.height > 0 {
                    let _ = chrome.set_bounds(chrome_rect(size));
                }
            }

            Event::UserEvent(Ev::Close)
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                save_glass(glass);
                *control_flow = ControlFlow::Exit;
            }

            _ => {}
        }
    });
}

/// Freezes the renderer. State is retained, so scroll position and typed text
/// survive; returns whether it actually suspended.
#[cfg(target_os = "windows")]
fn suspend(wv: &WebView) -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use webview2_com::TrySuspendCompletedHandler;
    use windows::core::Interface;
    use wry::{MemoryUsageLevel, WebViewExtWindows};

    if wv.set_visible(false).is_err() {
        return false;
    }
    let _ = wv.set_memory_usage_level(MemoryUsageLevel::Low);
    match wv.webview().cast::<ICoreWebView2_3>() {
        Ok(w3) => unsafe {
            w3.TrySuspend(&TrySuspendCompletedHandler::create(Box::new(|_, _| Ok(()))))
                .is_ok()
        },
        Err(_) => {
            let _ = wv.set_visible(true);
            false
        }
    }
}

#[cfg(target_os = "windows")]
fn resume(wv: &WebView) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use windows::core::Interface;
    use wry::{MemoryUsageLevel, WebViewExtWindows};

    if let Ok(w3) = wv.webview().cast::<ICoreWebView2_3>() {
        unsafe {
            let _ = w3.Resume();
        }
    }
    let _ = wv.set_visible(true);
    let _ = wv.set_memory_usage_level(MemoryUsageLevel::Normal);
}

#[cfg(not(target_os = "windows"))]
fn suspend(_: &WebView) -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
fn resume(_: &WebView) {}
