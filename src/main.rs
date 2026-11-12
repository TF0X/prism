#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ublock;

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

/// Chrome overlays the top of the window. The content webview cannot be inset
/// below it: only a *non-child* webview composites with the acrylic backdrop,
/// and wry force-resizes those to the full window on every WM_SIZE (its
/// set_bounds calls SetWindowPos on the app window itself). So the page is
/// pushed down from inside instead -- see the offset in glass.js.
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
    Shield(bool, String),
    /// Ctrl+W: the keyboard has no tab id, so it always means the active one.
    CloseTabActive,
}

struct Tab {
    id: u32,
    url: String,
    title: String,
}

pub fn state_dir() -> PathBuf {
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

/// Chrome's strip in *screen* coordinates. It is its own top-level window, so
/// a child webview cannot composite with the acrylic backdrop any more than a
/// tab could; giving it a window of its own is what makes the bar see-through.
fn chrome_geom(main: &Window) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
    let size = main.inner_size();
    let origin = main.inner_position().unwrap_or(PhysicalPosition::new(0, 0));
    (
        PhysicalPosition::new(origin.x, origin.y),
        PhysicalSize::new(size.width.max(1), CHROME_H),
    )
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
        backdrop(&window);
    }

    let ctx: &'static mut WebContext =
        // Named apart from a plain "profile": wry warns that webviews differing
        // in their browser-extension setting must not share a data directory.
        Box::leak(Box::new(WebContext::new(Some(
            state_dir().join("profile-ext"),
        ))));

    // Nothing is unpacked on a cold start, and an extension can only be
    // registered as the webview is built -- so that one case is worth the wait
    // rather than shipping an unprotected first session.
    ublock::ensure_first_run();
    ublock::spawn_sync(proxy.clone());

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
        cb = cb
            .with_browser_accelerator_keys(false)
            // wry loads every immediate subfolder of this path as an unpacked
            // extension, re-adding each launch (same id simply replaces).
            .with_browser_extensions_enabled(true)
            .with_extensions_path(ublock::root());
    }

    let content = cb.build(&window)?;
    accelerators(&content, &proxy);

    // Chrome lives in its own borderless window owned by the main one, for the
    // same reason tabs would: a child webview gets an opaque surface, so the
    // bar could never be see-through. Its own window also keeps it on a
    // separate origin, which is what stops a page spoofing the address bar.
    let (cpos, csize) = chrome_geom(&window);
    #[allow(unused_mut)]
    let mut cwb = WindowBuilder::new()
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_inner_size(csize)
        .with_position(cpos);

    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
        cwb = cwb
            .with_owner_window(window.hwnd())
            .with_skip_taskbar(true)
            .with_undecorated_shadow(false);
    }

    let chrome_win = cwb.build(&event_loop).unwrap();

    #[cfg(target_os = "windows")]
    {
        backdrop(&chrome_win);
    }

    let p_chrome = proxy.clone();
    let chrome = WebViewBuilder::new()
        .with_transparent(true)
        .with_custom_protocol("prism".into(), |_id, req| serve(&req))
        .with_url("prism://localhost/index.html")
        .with_ipc_handler(move |r: Request<String>| {
            if let Some(ev) = parse(r.body(), true) {
                let _ = p_chrome.send_event(ev);
            }
        })
        .build(&chrome_win)?;
    accelerators(&chrome, &proxy);

    let main_id = window.id();
    let mut tabs: Vec<Tab> = vec![Tab {
        id: 1,
        url: HOME.to_owned(),
        title: String::new(),
    }];
    let mut active: u32 = 1;
    let mut shield: Option<(bool, String)> = None;
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
                if let Some((on, label)) = &shield {
                    let _ = chrome.evaluate_script(&format!(
                        "window.__prism&&window.__prism.shield({on},{})",
                        serde_json::Value::String(label.clone())
                    ));
                }
            }

            Event::UserEvent(Ev::Shield(on, label)) => {
                let _ = chrome.evaluate_script(&format!(
                    "window.__prism&&window.__prism.shield({on},{})",
                    serde_json::Value::String(label.clone())
                ));
                shield = Some((on, label));
            }

            Event::UserEvent(Ev::CloseTabActive) => {
                let _ = proxy.send_event(Ev::CloseTab(active));
            }

            Event::UserEvent(Ev::PageReady) => {
                let _ = content.evaluate_script(&format!(
                    "window.__prismGlass&&window.__prismGlass({glass},{CHROME_H})"
                ));
                if std::env::var_os("PRISM_DIAG").is_some() {
                    // Probe after the page has settled its late CSS.
                    let _ = content.evaluate_script(
                        "setTimeout(function(){window.__prismProbe&&window.__prismProbe()},4000)",
                    );
                }
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

            // Chrome is top-level now, so it does not follow the main window.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Resized(size),
                ..
            } if window_id == main_id => {
                if size.width > 0 && size.height > 0 {
                    let (p, s) = chrome_geom(&window);
                    chrome_win.set_outer_position(p);
                    chrome_win.set_inner_size(s);
                }
            }

            Event::WindowEvent {
                window_id,
                event: WindowEvent::Moved(_),
                ..
            } if window_id == main_id => {
                let (p, s) = chrome_geom(&window);
                chrome_win.set_outer_position(p);
                chrome_win.set_inner_size(s);
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

/// Routes Ctrl shortcuts to us even while focus is inside a page.
///
/// tao never sees these: the keystroke belongs to the WebView2 controller, so
/// it has to be intercepted there. Registered on every webview, chrome and
/// content alike, and `SetHandled(true)` swallows the key so the page below
/// does not also act on it.
#[cfg(target_os = "windows")]
fn accelerators(wv: &WebView, proxy: &EventLoopProxy<Ev>) {
    use webview2_com::AcceleratorKeyPressedEventHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
    use wry::WebViewExtWindows;

    let proxy = proxy.clone();
    let mut token = 0i64;
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        unsafe {
            let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN;
            args.KeyEventKind(&mut kind)?;
            if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN {
                return Ok(());
            }
            // Modifiers are not in the args; read the live keyboard state.
            if GetKeyState(VK_CONTROL.0 as i32) >= 0 {
                return Ok(());
            }
            let mut vk = 0u32;
            args.VirtualKey(&mut vk)?;
            let ev = match vk {
                0x54 => Ev::NewTab(None),  // T
                0x57 => Ev::CloseTabActive, // W
                0x4C => Ev::FocusUrl,      // L
                0x52 => Ev::Reload,        // R
                _ => return Ok(()),
            };
            let _ = proxy.send_event(ev);
            args.SetHandled(true)?;
        }
        Ok(())
    }));

    unsafe {
        let _ = wv.controller().add_AcceleratorKeyPressed(&handler, &mut token);
    }
}

#[cfg(not(target_os = "windows"))]
fn accelerators(_: &WebView, _: &EventLoopProxy<Ev>) {}

/// What sits behind the page: a real Gaussian blur of the desktop, with a tint
/// we choose (so it is *not* forced dark the way Win11 acrylic is).
///
/// This calls the undocumented SetWindowCompositionAttribute with accent state
/// ACCENT_ENABLE_ACRYLICBLURBEHIND directly. window-vibrancy's apply_acrylic
/// will not do this on build 22621+ -- it switches to the DWM system backdrop,
/// whose tint is fixed dark -- and apply_blur's lighter blur-behind simply does
/// not render on recent Windows 11 at all. The accent-state acrylic still
/// blurs, at the cost of some lag while resizing. `PRISM_NOBLUR=1` disables it.
///
/// gradient_color is 0xAABBGGRR: alpha then blue, green, red.
#[cfg(target_os = "windows")]
fn backdrop(win: &Window) {
    use std::ffi::c_void;
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;

    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        gradient_color: u32,
        animation_id: u32,
    }
    #[repr(C)]
    struct CompAttrData {
        attribute: u32,
        data: *mut c_void,
        size: usize,
    }

    // Undocumented: not in user32's import library, so resolve it at runtime.
    type SetWca = unsafe extern "system" fn(HWND, *mut CompAttrData) -> i32;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::core::{s, w};

    let set_wca: SetWca = unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return;
        };
        match GetProcAddress(user32, s!("SetWindowCompositionAttribute")) {
            Some(p) => std::mem::transmute::<_, SetWca>(p),
            None => return,
        }
    };

    const WCA_ACCENT_POLICY: u32 = 19;
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
    const ACCENT_DISABLED: u32 = 0;

    let disabled = std::env::var_os("PRISM_NOBLUR").is_some();
    let mut accent = AccentPolicy {
        state: if disabled {
            ACCENT_DISABLED
        } else {
            ACCENT_ENABLE_ACRYLICBLURBEHIND
        },
        flags: 2, // draw the accent over the whole client area
        // Light, low-alpha frost: enough to read text over, nowhere near the
        // opaque dark of Win11's own acrylic. Low alpha => more see-through.
        gradient_color: 0x0D_1E_1A_16, // A=0x0D (~0.05), B=0x1E, G=0x1A, R=0x16
        animation_id: 0,
    };
    let mut data = CompAttrData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut accent as *mut _ as *mut c_void,
        size: std::mem::size_of::<AccentPolicy>(),
    };
    unsafe {
        let hwnd = HWND(win.hwnd() as *mut c_void);
        set_wca(hwnd, &mut data);
    }
}

#[cfg(not(target_os = "windows"))]
fn backdrop(_: &Window) {}

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
