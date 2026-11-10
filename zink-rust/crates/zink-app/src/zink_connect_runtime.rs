#[cfg(windows)]
use std::path::{Path, PathBuf};

#[cfg(windows)]
#[derive(Clone, Debug)]
enum BrowserCommand {
    Back,
    Forward,
    Refresh,
    Home,
    Navigate(String),
    NewTab(String),
    CloseTab(usize),
    SelectTab(usize),
    PageLoaded(usize, String),
    TitleChanged(usize, String),
}

#[cfg(windows)]
struct BrowserTab {
    id: usize,
    title: String,
    url: String,
    view: wry::WebView,
}

#[cfg(windows)]
fn browser_bounds(width: u32, height: u32) -> wry::Rect {
    use tao::dpi::{LogicalPosition, LogicalSize};
    const CHROME_HEIGHT: u32 = 204;
    wry::Rect {
        position: LogicalPosition::new(0, CHROME_HEIGHT).into(),
        size: LogicalSize::new(width, height.saturating_sub(CHROME_HEIGHT)).into(),
    }
}

#[cfg(windows)]
fn find_extension_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    for ancestor in executable.ancestors() {
        for relative in [
            "Extensions/uBlockOrigin/uBlock0.chromium",
            "Extensions/uBlockOrigin",
        ] {
            let candidate = ancestor.join(relative);
            if candidate.join("manifest.json").is_file() {
                return Some(candidate);
            }
        }
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../Extensions/uBlockOrigin/uBlock0.chromium");
    source.join("manifest.json").is_file().then_some(source)
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn create_tab(
    window: &tao::window::Window,
    context: &mut wry::WebContext,
    proxy: &tao::event_loop::EventLoopProxy<BrowserCommand>,
    id: usize,
    url: &str,
    width: u32,
    height: u32,
    extension_path: Option<&Path>,
) -> wry::Result<BrowserTab> {
    use wry::WebViewBuilderExtWindows;
    use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder};
    let load_proxy = proxy.clone();
    let title_proxy = proxy.clone();
    let popup_proxy = proxy.clone();
    let mut builder = WebViewBuilder::new_with_web_context(context)
        .with_bounds(browser_bounds(width, height))
        .with_url(url)
        .with_autoplay(true)
        .with_initialization_script(include_str!("bin/zink-connect-page.js"))
        .with_on_page_load_handler(move |event, loaded_url| {
            if matches!(event, PageLoadEvent::Finished) {
                let _ = load_proxy.send_event(BrowserCommand::PageLoaded(id, loaded_url));
            }
        })
        .with_document_title_changed_handler(move |title| {
            let _ = title_proxy.send_event(BrowserCommand::TitleChanged(id, title));
        })
        .with_new_window_req_handler(move |target, _| {
            let _ = popup_proxy.send_event(BrowserCommand::NewTab(target));
            NewWindowResponse::Deny
        })
        .with_browser_extensions_enabled(true);
    if let Some(path) = extension_path {
        builder = builder.with_extensions_path(path);
    }
    let view = builder.build_as_child(window)?;
    Ok(BrowserTab {
        id,
        title: "New tab".into(),
        url: url.into(),
        view,
    })
}

#[cfg(windows)]
fn update_toolbar(toolbar: &wry::WebView, tabs: &[BrowserTab], active: usize) {
    let tab_json = tabs
        .iter()
        .map(|tab| {
            format!(
                r#"{{"id":{},"title":{},"url":{}}}"#,
                tab.id,
                serde_json::to_string(&tab.title).unwrap_or_else(|_| "\"New tab\"".into()),
                serde_json::to_string(&tab.url).unwrap_or_else(|_| "\"\"".into())
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let active_id = tabs.get(active).map_or(0, |tab| tab.id);
    let _ = toolbar.evaluate_script(&format!(
        "window.zinkRenderTabs([{tab_json}], {active_id});"
    ));
}

#[cfg(windows)]
#[allow(clippy::too_many_lines)]
pub(crate) fn run() -> wry::Result<()> {
    run_internal(true, None)
}

#[cfg(windows)]
#[allow(clippy::too_many_lines)]
fn run_internal(visible: bool, ready: Option<std::sync::mpsc::SyncSender<()>>) -> wry::Result<()> {
    use tao::platform::windows::EventLoopBuilderExtWindows;
    use tao::platform::windows::WindowBuilderExtWindows;
    use tao::{
        dpi::{LogicalPosition, LogicalSize},
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder},
        window::{Theme, WindowBuilder},
    };
    use wry::{Rect, WebContext, WebViewBuilder, WebViewBuilderExtWindows};
    const CHROME_HEIGHT: u32 = 204;
    let mut event_loop_builder = EventLoopBuilder::<BrowserCommand>::with_user_event();
    event_loop_builder.with_any_thread(true);
    let event_loop = event_loop_builder.build();
    let proxy = event_loop.create_proxy();
    unsafe {
        let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
            windows::core::w!("Zink.App"),
        );
    }
    let owner = std::env::args_os()
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == "--owner-hwnd")
        .and_then(|pair| pair[1].to_string_lossy().parse::<isize>().ok());
    let mut window_builder = WindowBuilder::new()
        .with_title("Zink Connect")
        .with_theme(Some(Theme::Dark))
        .with_inner_size(LogicalSize::new(1440.0, 900.0))
        .with_maximized(true)
        .with_visible(visible);
    if let Some(owner) = owner {
        window_builder = window_builder.with_owner_window(owner);
    }
    let window = window_builder
        .build(&event_loop)
        .expect("create Zink Connect window");
    if visible {
        window.set_focus();
    }
    let initial = window.inner_size().to_logical::<u32>(window.scale_factor());
    let data_dir = std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("Zink/Connect/WebView2");
    let mut context = WebContext::new(Some(data_dir));
    let extension_path = find_extension_path();
    let toolbar_proxy = proxy.clone();
    let mut toolbar_builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_bounds(Rect {
            position: LogicalPosition::new(0, 0).into(),
            size: LogicalSize::new(initial.width, CHROME_HEIGHT).into(),
        })
        .with_html(include_str!("bin/zink-connect-toolbar.html"))
        .with_ipc_handler(move |request| {
            let body = request.body();
            let command = if let Some(value) = body.strip_prefix("navigate:") {
                BrowserCommand::Navigate(value.to_owned())
            } else if let Some(value) = body.strip_prefix("select:").and_then(|v| v.parse().ok()) {
                BrowserCommand::SelectTab(value)
            } else if let Some(value) = body.strip_prefix("close:").and_then(|v| v.parse().ok()) {
                BrowserCommand::CloseTab(value)
            } else {
                match body.as_str() {
                    "back" => BrowserCommand::Back,
                    "forward" => BrowserCommand::Forward,
                    "refresh" => BrowserCommand::Refresh,
                    "home" => BrowserCommand::Home,
                    "new-tab" => BrowserCommand::NewTab("https://www.bing.com/".into()),
                    "youtube" => BrowserCommand::Navigate("https://www.youtube.com/".into()),
                    "outlook" => BrowserCommand::Navigate("https://outlook.com/".into()),
                    "twitch" => BrowserCommand::Navigate("https://www.twitch.tv/".into()),
                    _ => return,
                }
            };
            let _ = toolbar_proxy.send_event(command);
        })
        .with_browser_extensions_enabled(true);
    if let Some(path) = extension_path.as_deref() {
        toolbar_builder = toolbar_builder.with_extensions_path(path);
    }
    let toolbar = toolbar_builder.build_as_child(&window)?;
    let first = create_tab(
        &window,
        &mut context,
        &proxy,
        1,
        "https://www.bing.com/",
        initial.width,
        initial.height,
        extension_path.as_deref(),
    )?;
    let mut tabs = vec![first];
    let mut active = 0usize;
    let mut next_id = 2usize;
    update_toolbar(&toolbar, &tabs, active);
    if let Some(ready) = ready {
        let _ = ready.send(());
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(command) => match command {
                BrowserCommand::Back => {
                    let _ = tabs[active].view.go_back();
                }
                BrowserCommand::Forward => {
                    let _ = tabs[active].view.go_forward();
                }
                BrowserCommand::Refresh => {
                    let _ = tabs[active].view.reload();
                }
                BrowserCommand::Home => {
                    let _ = tabs[active].view.load_url("https://www.bing.com/");
                }
                BrowserCommand::Navigate(url) => {
                    let _ = tabs[active].view.load_url(&url);
                }
                BrowserCommand::NewTab(url) => {
                    let size = window.inner_size().to_logical::<u32>(window.scale_factor());
                    if let Ok(tab) = create_tab(
                        &window,
                        &mut context,
                        &proxy,
                        next_id,
                        &url,
                        size.width,
                        size.height,
                        extension_path.as_deref(),
                    ) {
                        tabs[active].view.set_visible(false).ok();
                        tabs.push(tab);
                        active = tabs.len() - 1;
                        next_id += 1;
                        update_toolbar(&toolbar, &tabs, active);
                    }
                }
                BrowserCommand::SelectTab(id) => {
                    if let Some(index) = tabs.iter().position(|tab| tab.id == id) {
                        tabs[active].view.set_visible(false).ok();
                        active = index;
                        tabs[active].view.set_visible(true).ok();
                        update_toolbar(&toolbar, &tabs, active);
                    }
                }
                BrowserCommand::CloseTab(id) => {
                    if let Some(index) = tabs.iter().position(|tab| tab.id == id) {
                        let was_active = index == active;
                        tabs.remove(index);
                        if tabs.is_empty() {
                            let _ = proxy
                                .send_event(BrowserCommand::NewTab("https://www.bing.com/".into()));
                            active = 0;
                        } else {
                            active = if was_active {
                                index.saturating_sub(1).min(tabs.len() - 1)
                            } else if index < active {
                                active - 1
                            } else {
                                active
                            };
                            for (i, tab) in tabs.iter().enumerate() {
                                tab.view.set_visible(i == active).ok();
                            }
                            update_toolbar(&toolbar, &tabs, active);
                        }
                    }
                }
                BrowserCommand::PageLoaded(id, url) => {
                    if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                        tab.url = url;
                        update_toolbar(&toolbar, &tabs, active);
                    }
                }
                BrowserCommand::TitleChanged(id, title) => {
                    if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                        tab.title = if title.trim().is_empty() {
                            "New tab".into()
                        } else {
                            title
                        };
                        update_toolbar(&toolbar, &tabs, active);
                    }
                }
            },
            Event::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                let size = size.to_logical::<u32>(window.scale_factor());
                toolbar
                    .set_bounds(Rect {
                        position: LogicalPosition::new(0, 0).into(),
                        size: LogicalSize::new(size.width, CHROME_HEIGHT).into(),
                    })
                    .ok();
                for tab in &tabs {
                    tab.view
                        .set_bounds(browser_bounds(size.width, size.height))
                        .ok();
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

#[cfg(not(windows))]
pub(crate) fn run() -> wry::Result<()> {
    Ok(())
}
