#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::{ResizeDirection, Window, WindowBuilder},
};
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    http::{Request, Response},
};

const CHROME_HTML: &str = include_str!("chrome.html");
const GLASS_JS: &str = include_str!("glass.js");

/// Height of our own chrome strip, and the glass frame around the content.
const CHROME_H: u32 = 76;
const EDGE: u32 = 6;

const HOME: &str = "https://duckduckgo.com";
/// Background tabs freeze after this long untouched.
const IDLE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(15);

enum Ev {
    ChromeReady,
    TabReady(u32),
    NewTab(Option<String>),
    CloseTab(u32),
    SelectTab(u32),
    Navigate(String),
    Back,
    Forward,
    Reload,
    GlassCycle,
    Title(u32, String),
    Load(u32, bool, String),
    FocusUrl,
    Drag,
    Resize(ResizeDirection),
    Minimize,
    Close,
}

struct Tab {
    id: u32,
    view: WebView,
    title: String,
    url: String,
    loading: bool,
    /// Last time this tab was the active one; drives the freeze timer.
    touched: Instant,
    frozen: bool,
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

/// Turns whatever was typed into the address bar into a URL. Anything that is
/// not plausibly a host becomes a search, the way an address bar should behave.
fn to_url(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return HOME.to_owned();
    }
    if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("file://") {
        return t.to_owned();
    }
    let first = t.split('/').next().unwrap_or(t);
    let looks_like_host =
        !t.contains(' ') && first.contains('.') && !first.starts_with('.') && !first.ends_with('.');
    if looks_like_host || t.starts_with("localhost") {
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

fn content_rect(size: PhysicalSize<u32>) -> Rect {
    Rect {
        position: PhysicalPosition::new(EDGE as i32, CHROME_H as i32).into(),
        size: PhysicalSize::new(
            size.width.saturating_sub(EDGE * 2).max(1),
            size.height.saturating_sub(CHROME_H + EDGE).max(1),
        )
        .into(),
    }
}

/// Parses an IPC message into an event. Shared by the chrome webview and every
/// tab, with `tab` set for the latter so page-side messages carry their origin.
fn parse(body: &str, tab: Option<u32>) -> Option<Ev> {
    let m: serde_json::Value = serde_json::from_str(body).ok()?;
    let url = || m["url"].as_str().unwrap_or_default().to_owned();
    let id = || m["id"].as_u64().unwrap_or_default() as u32;
    Some(match m["action"].as_str()? {
        "ready" => match tab {
            Some(t) => Ev::TabReady(t),
            None => Ev::ChromeReady,
        },
        "navigate" => Ev::Navigate(m["text"].as_str().unwrap_or(&url()).to_owned()),
        "newtab" => Ev::NewTab(None),
        "closetab" => Ev::CloseTab(id()),
        "selecttab" => Ev::SelectTab(id()),
        "back" => Ev::Back,
        "forward" => Ev::Forward,
        "reload" => Ev::Reload,
        "glasscycle" => Ev::GlassCycle,
        "focusurl" => Ev::FocusUrl,
        "drag" => Ev::Drag,
        "minimize" => Ev::Minimize,
        "close" => Ev::Close,
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
        _ => return None,
    })
}

fn new_tab(
    window: &Window,
    ctx: &'static mut WebContext,
    proxy: &EventLoopProxy<Ev>,
    id: u32,
    url: &str,
    bounds: Rect,
) -> wry::Result<WebView> {
    let (p_ipc, p_title, p_load, p_new) = (
        proxy.clone(),
        proxy.clone(),
        proxy.clone(),
        proxy.clone(),
    );

    let mut b = WebViewBuilder::new_with_web_context(ctx)
        .with_bounds(bounds)
        .with_url(url)
        .with_transparent(true)
        .with_initialization_script(GLASS_JS)
        .with_ipc_handler(move |r: Request<String>| {
            if let Some(ev) = parse(r.body(), Some(id)) {
                let _ = p_ipc.send_event(ev);
            }
        })
        .with_document_title_changed_handler(move |t| {
            let _ = p_title.send_event(Ev::Title(id, t));
        })
        .with_on_page_load_handler(move |ev, url| {
            let _ = p_load.send_event(Ev::Load(id, matches!(ev, PageLoadEvent::Finished), url));
        })
        // Deny, then open it ourselves: Create would require sharing the
        // WebView2 environment, and we want it as a tab regardless.
        .with_new_window_req_handler(move |url, _| {
            let _ = p_new.send_event(Ev::NewTab(Some(url)));
            NewWindowResponse::Deny
        });

    #[cfg(target_os = "windows")]
    {
        use wry::WebViewBuilderExtWindows;
        // Stop WebView2's built-in shortcuts firing under our own.
        b = b.with_browser_accelerator_keys(false);
    }

    b.build_as_child(window)
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

    // One context for every tab, so there is a single cookie jar and logins
    // persist. Leaked because the builder borrows it for 'a and it must outlive
    // every webview anyway.
    let ctx: &'static mut WebContext =
        Box::leak(Box::new(WebContext::new(Some(state_dir().join("profile")))));
    let ctx_ptr: *mut WebContext = ctx;

    let size = window.inner_size();
    let p_chrome = proxy.clone();

    let chrome = WebViewBuilder::new()
        .with_bounds(Rect {
            position: PhysicalPosition::new(0, 0).into(),
            size: PhysicalSize::new(size.width, size.height).into(),
        })
        .with_transparent(true)
        .with_custom_protocol("prism".into(), |_id, req| serve(&req))
        .with_url("prism://localhost/index.html")
        .with_ipc_handler(move |r: Request<String>| {
            if let Some(ev) = parse(r.body(), None) {
                let _ = p_chrome.send_event(ev);
            }
        })
        .build_as_child(&window)?;

    let mut tabs: Vec<Tab> = Vec::new();
    let mut active: u32 = 0;
    let mut next_id: u32 = 1;
    let mut glass = load_glass();

    let _ = proxy.send_event(Ev::NewTab(None));

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);

        // Pushes the whole tab list into the chrome UI.
        macro_rules! sync_tabs {
            () => {{
                let list: Vec<serde_json::Value> = tabs
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "title": t.title,
                            "active": t.id == active,
                            "loading": t.loading,
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
                let (url, back, fwd) = match tabs.iter().find(|t| t.id == active) {
                    Some(t) => (
                        t.url.clone(),
                        t.view.can_go_back().unwrap_or(false),
                        t.view.can_go_forward().unwrap_or(false),
                    ),
                    None => (String::new(), false, false),
                };
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.nav({})",
                    serde_json::json!({
                        "url": url,
                        "secure": url.starts_with("https://"),
                        "canBack": back,
                        "canFwd": fwd,
                    })
                ));
            }};
        }

        macro_rules! show_active {
            () => {{
                for t in tabs.iter_mut() {
                    let on = t.id == active;
                    let _ = t.view.set_visible(on);
                    if on {
                        t.touched = Instant::now();
                        if t.frozen {
                            glass_resume(&t.view);
                            t.frozen = false;
                        }
                    }
                }
            }};
        }

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                for t in tabs.iter_mut() {
                    if t.id != active && !t.frozen && t.touched.elapsed() >= IDLE {
                        t.frozen = glass_suspend(&t.view);
                    }
                }
            }

            Event::UserEvent(Ev::ChromeReady) => {
                sync_tabs!();
                sync_nav!();
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.glass({},'{}')",
                    glass,
                    ["Off", "Frosted", "Clear"][glass as usize]
                ));
            }

            Event::UserEvent(Ev::TabReady(id)) => {
                if let Some(t) = tabs.iter().find(|t| t.id == id) {
                    let _ = t.view.evaluate_script(&format!("window.__prismGlass({glass})"));
                }
            }

            Event::UserEvent(Ev::NewTab(url)) => {
                let id = next_id;
                next_id += 1;
                let target = url.unwrap_or_else(|| HOME.to_owned());
                let bounds = content_rect(window.inner_size());
                // SAFETY: single-threaded event loop; the context is leaked and
                // outlives every webview built from it.
                let ctx_ref: &'static mut WebContext = unsafe { &mut *ctx_ptr };
                match new_tab(&window, ctx_ref, &proxy, id, &target, bounds) {
                    Ok(view) => {
                        tabs.push(Tab {
                            id,
                            view,
                            title: String::new(),
                            url: target,
                            loading: true,
                            touched: Instant::now(),
                            frozen: false,
                        });
                        active = id;
                        show_active!();
                        sync_tabs!();
                        sync_nav!();
                    }
                    Err(e) => eprintln!("tab failed: {e}"),
                }
            }

            Event::UserEvent(Ev::CloseTab(id)) => {
                if let Some(i) = tabs.iter().position(|t| t.id == id) {
                    let t = tabs.remove(i);
                    let _ = t.view.set_visible(false);
                    drop(t);
                    if tabs.is_empty() {
                        let _ = proxy.send_event(Ev::NewTab(None));
                    } else {
                        if active == id {
                            active = tabs[i.min(tabs.len() - 1)].id;
                        }
                        show_active!();
                        sync_tabs!();
                        sync_nav!();
                    }
                }
            }

            Event::UserEvent(Ev::SelectTab(id)) => {
                if tabs.iter().any(|t| t.id == id) {
                    active = id;
                    show_active!();
                    sync_tabs!();
                    sync_nav!();
                }
            }

            Event::UserEvent(Ev::Navigate(text)) => {
                let url = to_url(&text);
                let mut changed = false;
                if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                    t.loading = true;
                    let _ = t.view.load_url(&url);
                    changed = true;
                }
                if changed {
                    sync_tabs!();
                }
            }

            Event::UserEvent(Ev::Back) => {
                if let Some(t) = tabs.iter().find(|t| t.id == active) {
                    let _ = t.view.go_back();
                }
            }
            Event::UserEvent(Ev::Forward) => {
                if let Some(t) = tabs.iter().find(|t| t.id == active) {
                    let _ = t.view.go_forward();
                }
            }
            Event::UserEvent(Ev::Reload) => {
                if let Some(t) = tabs.iter().find(|t| t.id == active) {
                    let _ = t.view.reload();
                }
            }

            Event::UserEvent(Ev::GlassCycle) => {
                glass = (glass + 1) % 3;
                save_glass(glass);
                for t in tabs.iter() {
                    let _ = t.view.evaluate_script(&format!("window.__prismGlass({glass})"));
                }
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.glass({},'{}')",
                    glass,
                    ["Off", "Frosted", "Clear"][glass as usize]
                ));
            }

            Event::UserEvent(Ev::Title(id, title)) => {
                let mut changed = false;
                if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                    t.title = title;
                    changed = true;
                }
                if changed {
                    sync_tabs!();
                }
            }

            Event::UserEvent(Ev::Load(id, finished, url)) => {
                let mut changed = false;
                if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                    t.url = url;
                    t.loading = !finished;
                    changed = true;
                }
                if changed {
                    sync_tabs!();
                    if id == active {
                        sync_nav!();
                    }
                }
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
                event: WindowEvent::Resized(size),
                ..
            } => {
                if size.width > 0 && size.height > 0 {
                    let _ = chrome.set_bounds(Rect {
                        position: PhysicalPosition::new(0, 0).into(),
                        size: PhysicalSize::new(size.width, size.height).into(),
                    });
                    // Hidden tabs too, or they come back mis-sized.
                    let r = content_rect(size);
                    for t in tabs.iter() {
                        let _ = t.view.set_bounds(r);
                    }
                }
            }

            Event::UserEvent(Ev::Close)
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                save_glass(glass);
                tabs.clear();
                *control_flow = ControlFlow::Exit;
            }

            _ => {}
        }
    });
}

/// Freezes a background tab's renderer. State is retained, so scroll position
/// and half-typed input survive; returns whether it actually suspended.
#[cfg(target_os = "windows")]
fn glass_suspend(wv: &WebView) -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use webview2_com::TrySuspendCompletedHandler;
    use windows::core::Interface;
    use wry::{MemoryUsageLevel, WebViewExtWindows};

    let _ = wv.set_memory_usage_level(MemoryUsageLevel::Low);
    match wv.webview().cast::<ICoreWebView2_3>() {
        Ok(w3) => unsafe {
            w3.TrySuspend(&TrySuspendCompletedHandler::create(Box::new(|_, _| Ok(()))))
                .is_ok()
        },
        Err(_) => false,
    }
}

#[cfg(target_os = "windows")]
fn glass_resume(wv: &WebView) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use windows::core::Interface;
    use wry::{MemoryUsageLevel, WebViewExtWindows};

    if let Ok(w3) = wv.webview().cast::<ICoreWebView2_3>() {
        unsafe {
            let _ = w3.Resume();
        }
    }
    let _ = wv.set_memory_usage_level(MemoryUsageLevel::Normal);
}

#[cfg(not(target_os = "windows"))]
fn glass_suspend(_: &WebView) -> bool {
    false
}
#[cfg(not(target_os = "windows"))]
fn glass_resume(_: &WebView) {}
