use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use raw_window_handle::HasWindowHandle;
use slint::{ComponentHandle, Timer, TimerMode};
use wry::{
    NewWindowResponse, PageLoadEvent, WebContext, WebView, WebViewBuilder, WebViewExtWindows,
};

use crate::ZinkConnectWindow;
use zink_platform_windows::create_zink_connect_shortcut;

const HOME: &str = "https://www.bing.com/";
const CHROME_HEIGHT: f64 = 168.0;

#[derive(Clone, Debug)]
enum Command {
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
    Fullscreen(bool),
    XboxAchievement {
        title: String,
        score: String,
        description: String,
    },
    ShowInternalPage(String),
    CloseInternalPage,
    ClearBrowsingData,
    SetDeleteOnClose(bool),
    SetRememberSites(bool),
    SetLaunchBrowserOnly(bool),
    SetAdBlock(bool),
    SetWebViewUpdates(bool),
    CreateShortcut,
}

type CommandQueue = Rc<RefCell<VecDeque<Command>>>;

struct BrowserTab {
    id: usize,
    title: String,
    url: String,
    view: WebView,
}

#[allow(clippy::struct_excessive_bools)]
struct BrowserState {
    window: ZinkConnectWindow,
    context: WebContext,
    toolbar: WebView,
    tabs: Vec<BrowserTab>,
    active: usize,
    next_id: usize,
    queue: CommandQueue,
    last_size: (u32, u32),
    fullscreen: bool,
    internal_page: bool,
    delete_on_close: bool,
    remember_sites: bool,
    launch_browser_only: bool,
    ad_block_enabled: bool,
    ad_block_switch: Arc<AtomicBool>,
    ad_block_engine: Arc<crate::zink_ad_block::NativeAdBlock>,
    webview_updates_enabled: Arc<AtomicBool>,
}

pub(crate) struct ZinkConnect {
    state: Rc<RefCell<Option<BrowserState>>>,
    pending: Rc<RefCell<Option<ZinkConnectWindow>>>,
    pump: Timer,
    force_browser_only: Rc<Cell<bool>>,
    pending_url: Rc<RefCell<Option<String>>>,
    pending_internal_page: Rc<RefCell<Option<String>>>,
}

impl ZinkConnect {
    pub(crate) fn new() -> Self {
        let state: Rc<RefCell<Option<BrowserState>>> = Rc::new(RefCell::new(None));
        let pending: Rc<RefCell<Option<ZinkConnectWindow>>> = Rc::new(RefCell::new(None));
        let force_browser_only = Rc::new(Cell::new(false));
        let pending_url = Rc::new(RefCell::new(None::<String>));
        let pending_internal_page = Rc::new(RefCell::new(None::<String>));
        let pump = Timer::default();
        let state_for_pump = Rc::clone(&state);
        let pending_for_pump = Rc::clone(&pending);
        let force_for_pump = Rc::clone(&force_browser_only);
        let url_for_pump = Rc::clone(&pending_url);
        let internal_page_for_pump = Rc::clone(&pending_internal_page);
        pump.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let should_close = state_for_pump
                .borrow()
                .as_ref()
                .is_some_and(|browser| !browser.window.window().is_visible());
            if should_close {
                if let Some(browser) = state_for_pump.borrow().as_ref() {
                    browser.clear_close_data();
                    if browser.launch_browser_only {
                        let _ = slint::quit_event_loop();
                    } else {
                        set_main_zink_visible(true);
                    }
                }
                *state_for_pump.borrow_mut() = None;
            } else if let Some(browser) = state_for_pump.borrow_mut().as_mut() {
                browser.pump();
                return;
            }
            let pending_window = pending_for_pump.borrow_mut().take();
            if let Some(window) = pending_window {
                if window.window().window_handle().window_handle().is_err() {
                    *pending_for_pump.borrow_mut() = Some(window);
                    return;
                }
                match create_browser(window) {
                    Ok(mut browser) => {
                        if let Some(url) = url_for_pump.borrow_mut().take() {
                            let _ = browser.tabs[browser.active].view.load_url(&url);
                        }
                        if let Some(page) = internal_page_for_pump.borrow_mut().take() {
                            browser.show_internal_page(&page);
                        }
                        if force_for_pump.get() {
                            browser.launch_browser_only = true;
                            set_main_zink_visible(false);
                        }
                        *state_for_pump.borrow_mut() = Some(browser);
                    }
                    Err(error) => {
                        tracing::error!(%error, "failed to create in-process Zink Connect WebViews");
                    }
                }
            }
        });
        Self {
            state,
            pending,
            pump,
            force_browser_only,
            pending_url,
            pending_internal_page,
        }
    }

    pub(crate) fn open(&mut self) -> Result<()> {
        // Reading the timer keeps its ownership explicit: dropping it stops the command pump.
        let _ = &self.pump;
        let mut guard = self.state.borrow_mut();
        if let Some(browser) = guard.as_mut() {
            browser.window.show().context("show Zink Connect")?;
            browser.window.window().set_maximized(true);
            return Ok(());
        }
        drop(guard);
        if let Some(window) = self.pending.borrow().as_ref() {
            window.show().context("show Zink Connect")?;
            return Ok(());
        }

        let window = ZinkConnectWindow::new().context("create Zink Connect window")?;
        window.show().context("show Zink Connect")?;
        window.window().set_maximized(true);
        *self.pending.borrow_mut() = Some(window);
        Ok(())
    }

    pub(crate) fn open_browser_only(&mut self) -> Result<()> {
        self.force_browser_only.set(true);
        self.open()
    }

    pub(crate) fn open_internal_page(&mut self, page: &str) -> Result<()> {
        if !matches!(page, "settings" | "history") {
            anyhow::bail!("unknown Zink Connect page")
        }
        if let Some(browser) = self.state.borrow_mut().as_mut() {
            browser.window.show().context("show Zink Connect")?;
            browser.show_internal_page(page);
            return Ok(());
        }
        *self.pending_internal_page.borrow_mut() = Some(page.to_owned());
        self.open()
    }

    pub(crate) fn open_url(&mut self, url: &str) -> Result<()> {
        if let Some(browser) = self.state.borrow_mut().as_mut() {
            browser.tabs[browser.active]
                .view
                .load_url(url)
                .context("navigate Zink Connect")?;
            browser.window.show().context("show Zink Connect")?;
            browser.window.window().set_maximized(true);
            return Ok(());
        }
        *self.pending_url.borrow_mut() = Some(url.to_owned());
        self.open()
    }
}

fn create_browser(window: ZinkConnectWindow) -> Result<BrowserState> {
    let queue = Rc::new(RefCell::new(VecDeque::new()));
    let size = logical_size(&window);
    let data_dir = std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("Zink/Connect/WebView2");
    let mut context = WebContext::new(Some(data_dir));
    let ad_block_switch = Arc::new(AtomicBool::new(true));
    let ad_block_engine = Arc::new(crate::zink_ad_block::NativeAdBlock::new());
    let toolbar = create_toolbar(&window, &mut context, Rc::clone(&queue), size)
        .context("create Zink Connect toolbar WebView")?;
    let first = create_tab(
        &window,
        &mut context,
        Rc::clone(&queue),
        1,
        HOME,
        size,
        Arc::clone(&ad_block_switch),
        Arc::clone(&ad_block_engine),
    )
    .context("create Zink Connect page WebView")?;
    let webview_updates_enabled = Arc::new(AtomicBool::new(true));
    register_webview_update_notifications(&first.view, Arc::clone(&webview_updates_enabled))
        .context("register WebView2 Evergreen update notifications")?;
    let browser = BrowserState {
        window,
        context,
        toolbar,
        tabs: vec![first],
        active: 0,
        next_id: 2,
        queue,
        last_size: size,
        fullscreen: false,
        internal_page: false,
        delete_on_close: false,
        remember_sites: true,
        launch_browser_only: false,
        ad_block_enabled: true,
        ad_block_switch,
        ad_block_engine,
        webview_updates_enabled,
    };
    browser.update_toolbar();
    Ok(browser)
}

impl BrowserState {
    fn pump(&mut self) {
        let size = logical_size(&self.window);
        if size != self.last_size {
            self.resize(size);
            self.last_size = size;
        }
        loop {
            let command = self.queue.borrow_mut().pop_front();
            let Some(command) = command else { break };
            self.handle(command);
        }
        if let Some(message) = take_webview_update_status() {
            let message = serde_json::to_string(&message).unwrap_or_else(|_| "\"\"".into());
            let _ = self
                .toolbar
                .evaluate_script(&format!("window.zinkWebViewUpdateStatus({message});"));
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, command: Command) {
        match command {
            Command::Back => {
                let _ = self.tabs[self.active].view.go_back();
            }
            Command::Forward => {
                let _ = self.tabs[self.active].view.go_forward();
            }
            Command::Refresh => {
                let _ = self.tabs[self.active].view.reload();
            }
            Command::Home => {
                let _ = self.tabs[self.active].view.load_url(HOME);
            }
            Command::Navigate(url) => {
                let _ = self.tabs[self.active].view.load_url(&url);
            }
            Command::NewTab(url) => {
                self.set_fullscreen(false);
                if let Ok(tab) = create_tab(
                    &self.window,
                    &mut self.context,
                    Rc::clone(&self.queue),
                    self.next_id,
                    &url,
                    self.last_size,
                    Arc::clone(&self.ad_block_switch),
                    Arc::clone(&self.ad_block_engine),
                ) {
                    let enabled = self.ad_block_enabled;
                    let _ = tab
                        .view
                        .evaluate_script(&format!("window.__zinkSetAdBlock({enabled});"));
                    let _ = self.tabs[self.active].view.set_visible(false);
                    self.tabs.push(tab);
                    self.active = self.tabs.len() - 1;
                    self.next_id += 1;
                    self.update_toolbar();
                }
            }
            Command::SelectTab(id) => {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    self.set_fullscreen(false);
                    let _ = self.tabs[self.active].view.set_visible(false);
                    self.active = index;
                    let _ = self.tabs[self.active].view.set_visible(true);
                    self.update_toolbar();
                }
            }
            Command::CloseTab(id) => {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    self.set_fullscreen(false);
                    let was_active = index == self.active;
                    self.tabs.remove(index);
                    if self.tabs.is_empty() {
                        enqueue(&self.queue, Command::NewTab(HOME.into()));
                        self.active = 0;
                    } else {
                        self.active = if was_active {
                            index.saturating_sub(1).min(self.tabs.len() - 1)
                        } else if index < self.active {
                            self.active - 1
                        } else {
                            self.active
                        };
                        for (tab_index, tab) in self.tabs.iter().enumerate() {
                            let _ = tab.view.set_visible(tab_index == self.active);
                        }
                        self.update_toolbar();
                    }
                }
            }
            Command::PageLoaded(id, url) => {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
                    tab.url = url;
                }
                self.update_toolbar();
            }
            Command::TitleChanged(id, title) => {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
                    tab.title = if title.trim().is_empty() {
                        "New tab".into()
                    } else {
                        title
                    };
                }
                self.update_toolbar();
            }
            Command::Fullscreen(fullscreen) => self.set_fullscreen(fullscreen),
            Command::XboxAchievement {
                title,
                score,
                description,
            } => {
                if let Err(error) = crate::show_windows_toast(
                    &format!("Achievement unlocked • {score}"),
                    &format!("{title} — {description}"),
                ) {
                    tracing::error!(%error, "failed to show Xbox achievement notification");
                }
            }
            Command::ShowInternalPage(page) => self.show_internal_page(&page),
            Command::CloseInternalPage => self.hide_internal_page(),
            Command::ClearBrowsingData => {
                for tab in &self.tabs {
                    let _ = tab.view.clear_all_browsing_data();
                }
                let _ = self.toolbar.evaluate_script("window.zinkDataCleared();");
            }
            Command::SetDeleteOnClose(enabled) => self.delete_on_close = enabled,
            Command::SetRememberSites(enabled) => self.remember_sites = enabled,
            Command::SetLaunchBrowserOnly(enabled) => {
                if !self.launch_browser_only || enabled {
                    self.launch_browser_only = enabled;
                    set_main_zink_visible(!enabled);
                }
            }
            Command::SetAdBlock(enabled) => {
                self.ad_block_enabled = enabled;
                self.ad_block_switch.store(enabled, Ordering::Relaxed);
                for tab in &self.tabs {
                    let _ = tab
                        .view
                        .evaluate_script(&format!("window.__zinkSetAdBlock({enabled});"));
                }
            }
            Command::SetWebViewUpdates(enabled) => {
                self.webview_updates_enabled
                    .store(enabled, Ordering::Relaxed);
                if enabled {
                    let version = webview_runtime_version(&self.tabs[self.active].view)
                        .unwrap_or_else(|| "unknown".into());
                    let message = serde_json::to_string(&format!(
                        "Evergreen updates are active. Installed version: {version}. Microsoft Edge Update installs new WebView2 versions automatically; Zink will notify you when a restart is needed."
                    ))
                    .unwrap_or_else(|_| "\"WebView2 Evergreen updates are active.\"".into());
                    let _ = self
                        .toolbar
                        .evaluate_script(&format!("window.zinkWebViewUpdateStatus({message});"));
                } else {
                    let _ = self.toolbar.evaluate_script(
                        "window.zinkWebViewUpdateStatus('Zink update notifications are off. Microsoft may still update the Evergreen runtime in Windows.');",
                    );
                }
            }
            Command::CreateShortcut => {
                let message = match create_zink_connect_shortcut() {
                    Ok(_) => match zink_platform_windows::request_taskbar_pin() {
                        Ok(()) => "Confirm the Windows prompt to pin Zink Connect to the taskbar.",
                        Err(_) => {
                            "The shortcut was created in Start, but Windows cannot request taskbar pinning on this device."
                        }
                    },
                    Err(_) => "The Zink Connect shortcut could not be created.",
                };
                let message = serde_json::to_string(message).unwrap_or_else(|_| "\"\"".into());
                let _ = self
                    .toolbar
                    .evaluate_script(&format!("window.zinkShortcutResult({message});"));
            }
        }
    }

    fn resize(&self, size: (u32, u32)) {
        let bounds = if self.internal_page {
            fullscreen_bounds(size)
        } else {
            toolbar_bounds(size)
        };
        let _ = self.toolbar.set_bounds(bounds);
        for tab in &self.tabs {
            let bounds = if self.fullscreen && tab.id == self.tabs[self.active].id {
                fullscreen_bounds(size)
            } else {
                browser_bounds(size)
            };
            let _ = tab.view.set_bounds(bounds);
        }
    }

    fn set_fullscreen(&mut self, fullscreen: bool) {
        if self.fullscreen == fullscreen {
            return;
        }
        self.fullscreen = fullscreen;
        self.window.window().set_fullscreen(fullscreen);
        let _ = self.toolbar.set_visible(!fullscreen);
        let size = logical_size(&self.window);
        let bounds = if fullscreen {
            fullscreen_bounds(size)
        } else {
            browser_bounds(size)
        };
        let _ = self.tabs[self.active].view.set_bounds(bounds);
        if !fullscreen {
            let _ = self.toolbar.set_bounds(toolbar_bounds(size));
        }
        self.last_size = size;
    }

    fn show_internal_page(&mut self, page: &str) {
        self.set_fullscreen(false);
        self.internal_page = true;
        for tab in &self.tabs {
            let _ = tab.view.set_visible(false);
        }
        let size = logical_size(&self.window);
        let _ = self.toolbar.set_bounds(fullscreen_bounds(size));
        let page = serde_json::to_string(page).unwrap_or_else(|_| "\"settings\"".into());
        let _ = self
            .toolbar
            .evaluate_script(&format!("window.zinkShowInternalPage({page});"));
    }

    fn hide_internal_page(&mut self) {
        self.internal_page = false;
        let size = logical_size(&self.window);
        let _ = self.toolbar.set_bounds(toolbar_bounds(size));
        let _ = self
            .toolbar
            .evaluate_script("window.zinkHideInternalPages();");
        let _ = self.tabs[self.active].view.set_visible(true);
    }

    fn update_toolbar(&self) {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                format!(
                    r#"{{"id":{},"title":{},"url":{}}}"#,
                    tab.id,
                    serde_json::to_string(&tab.title).unwrap_or_else(|_| "\"New tab\"".into()),
                    serde_json::to_string(&tab.url).unwrap_or_else(|_| "\"\"".into()),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let active_id = self.tabs.get(self.active).map_or(0, |tab| tab.id);
        let _ = self
            .toolbar
            .evaluate_script(&format!("window.zinkRenderTabs([{tabs}], {active_id});"));
        if let Some(tab) = self.tabs.get(self.active) {
            let title = serde_json::to_string(&tab.title).unwrap_or_else(|_| "\"\"".into());
            let url = serde_json::to_string(&tab.url).unwrap_or_else(|_| "\"\"".into());
            let _ = self
                .toolbar
                .evaluate_script(&format!("window.zinkAddHistory({title}, {url});"));
        }
    }

    fn clear_close_data(&self) {
        if self.delete_on_close || !self.remember_sites {
            for tab in &self.tabs {
                let _ = tab.view.clear_all_browsing_data();
            }
        }
    }
}

fn create_toolbar(
    window: &ZinkConnectWindow,
    context: &mut WebContext,
    queue: CommandQueue,
    size: (u32, u32),
) -> wry::Result<WebView> {
    let builder = WebViewBuilder::new_with_web_context(context)
        .with_bounds(toolbar_bounds(size))
        .with_html(include_str!("bin/zink-connect-toolbar.html"))
        .with_ipc_handler(move |request| {
            let body = request.body();
            let command = if let Some(value) = body.strip_prefix("navigate:") {
                Command::Navigate(value.to_owned())
            } else if let Some(value) = body
                .strip_prefix("select:")
                .and_then(|value| value.parse().ok())
            {
                Command::SelectTab(value)
            } else if let Some(value) = body
                .strip_prefix("close:")
                .and_then(|value| value.parse().ok())
            {
                Command::CloseTab(value)
            } else if let Some(value) = body.strip_prefix("setting:delete-close:") {
                Command::SetDeleteOnClose(value == "true")
            } else if let Some(value) = body.strip_prefix("setting:remember-sites:") {
                Command::SetRememberSites(value == "true")
            } else if let Some(value) = body.strip_prefix("setting:launch-browser-only:") {
                Command::SetLaunchBrowserOnly(value == "true")
            } else if let Some(value) = body.strip_prefix("setting:ad-block-enabled:") {
                Command::SetAdBlock(value == "true")
            } else if let Some(value) = body.strip_prefix("setting:webview-updates-enabled:") {
                Command::SetWebViewUpdates(value == "true")
            } else {
                match body.as_str() {
                    "back" => Command::Back,
                    "forward" => Command::Forward,
                    "refresh" => Command::Refresh,
                    "home" => Command::Home,
                    "new-tab" => Command::NewTab(HOME.into()),
                    "youtube" => Command::Navigate("https://www.youtube.com/".into()),
                    "outlook" => Command::Navigate("https://outlook.com/".into()),
                    "twitch" => Command::Navigate("https://www.twitch.tv/".into()),
                    "settings" => Command::ShowInternalPage("settings".into()),
                    "history" => Command::ShowInternalPage("history".into()),
                    "close-internal" => Command::CloseInternalPage,
                    "clear-data" => Command::ClearBrowsingData,
                    "create-shortcut" => Command::CreateShortcut,
                    _ => return,
                }
            };
            enqueue(&queue, command);
        });
    let handle = window.window().window_handle();
    builder.build_as_child(&handle)
}

#[allow(clippy::too_many_arguments)]
fn create_tab(
    window: &ZinkConnectWindow,
    context: &mut WebContext,
    queue: CommandQueue,
    id: usize,
    url: &str,
    size: (u32, u32),
    ad_block_switch: Arc<AtomicBool>,
    ad_block_engine: Arc<crate::zink_ad_block::NativeAdBlock>,
) -> wry::Result<BrowserTab> {
    let loaded_queue = Rc::clone(&queue);
    let title_queue = Rc::clone(&queue);
    let fullscreen_queue = Rc::clone(&queue);
    let popup_queue = queue;
    let builder = WebViewBuilder::new_with_web_context(context)
        .with_bounds(browser_bounds(size))
        .with_url(url)
        .with_autoplay(true)
        .with_initialization_script(crate::zink_ad_block::initialization_script())
        .with_initialization_script(include_str!("bin/zink-connect-page.js"))
        .with_ipc_handler(move |request| {
            let message = request.body();
            match message.as_str() {
                "fullscreen:enter" => enqueue(&fullscreen_queue, Command::Fullscreen(true)),
                "fullscreen:exit" => enqueue(&fullscreen_queue, Command::Fullscreen(false)),
                _ => {
                    if let Some(payload) = message.strip_prefix("xbox-achievement:")
                        && let Ok(value) = serde_json::from_str::<serde_json::Value>(payload)
                    {
                        enqueue(
                            &fullscreen_queue,
                            Command::XboxAchievement {
                                title: value["title"]
                                    .as_str()
                                    .unwrap_or("Xbox achievement")
                                    .to_owned(),
                                score: value["score"].as_str().unwrap_or("G").to_owned(),
                                description: value["description"]
                                    .as_str()
                                    .unwrap_or("Xbox achievement added to your profile")
                                    .to_owned(),
                            },
                        );
                    }
                }
            }
        })
        .with_on_page_load_handler(move |event, url| {
            if matches!(event, PageLoadEvent::Finished) {
                enqueue(&loaded_queue, Command::PageLoaded(id, url));
            }
        })
        .with_document_title_changed_handler(move |title| {
            enqueue(&title_queue, Command::TitleChanged(id, title));
        })
        .with_new_window_req_handler(move |target, _| {
            enqueue(&popup_queue, Command::NewTab(target));
            NewWindowResponse::Deny
        });
    let handle = window.window().window_handle();
    let view = builder.build_as_child(&handle)?;
    if let Err(error) = install_network_ad_filter(&view, ad_block_switch, ad_block_engine) {
        tracing::error!(%error, "failed to install Zink Connect network ad filter");
    }
    Ok(BrowserTab {
        id,
        title: "New tab".into(),
        url: url.into(),
        view,
    })
}

fn install_network_ad_filter(
    view: &WebView,
    enabled: Arc<AtomicBool>,
    engine: Arc<crate::zink_ad_block::NativeAdBlock>,
) -> anyhow::Result<()> {
    use webview2_com::{
        Microsoft::Web::WebView2::Win32::COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
        WebResourceRequestedEventHandler,
    };
    use windows::core::{HSTRING, PWSTR, w};

    let webview = view.webview();
    let environment = view.environment();
    // SAFETY: The WebView2 handler is registered and invoked on its owning UI thread.
    unsafe {
        webview.AddWebResourceRequestedFilter(
            &HSTRING::from("*"),
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
        )?;
        let mut token = 0_i64;
        webview.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, arguments| {
                let Some(arguments) = arguments else {
                    return Ok(());
                };
                if !enabled.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let request = arguments.Request()?;
                let mut raw_uri = PWSTR::null();
                request.Uri(&raw mut raw_uri)?;
                let uri = raw_uri.to_string()?;
                windows::Win32::System::Com::CoTaskMemFree(Some(raw_uri.as_ptr().cast()));
                let headers = request.Headers()?;
                let mut raw_referer = PWSTR::null();
                let source = if headers
                    .GetHeader(w!("Referer"), &raw mut raw_referer)
                    .is_ok()
                {
                    let value = raw_referer.to_string().unwrap_or_else(|_| uri.clone());
                    windows::Win32::System::Com::CoTaskMemFree(Some(raw_referer.as_ptr().cast()));
                    value
                } else {
                    uri.clone()
                };
                let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
                arguments.ResourceContext(&raw mut context)?;
                let resource_type = web_resource_type(context);
                if engine.should_block(&uri, &source, resource_type) {
                    let response = environment.CreateWebResourceResponse(
                        None,
                        204,
                        &HSTRING::from("No Content"),
                        &HSTRING::from("Cache-Control: no-store\r\n"),
                    )?;
                    arguments.SetResponse(&response)?;
                }
                Ok(())
            })),
            &raw mut token,
        )?;
    }
    Ok(())
}

fn web_resource_type(
    context: webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_WEB_RESOURCE_CONTEXT,
) -> &'static str {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
    };
    match context {
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => "document",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => "fetch",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST => "xmlhttprequest",
        _ => "other",
    }
}

fn webview_runtime_version(view: &WebView) -> Option<String> {
    use windows::core::PWSTR;

    let environment = view.environment();
    let mut value = PWSTR::null();
    // SAFETY: WebView2 allocates the returned string with COM task memory.
    unsafe {
        environment.BrowserVersionString(&raw mut value).ok()?;
        let version = value.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(value.as_ptr().cast()));
        version
    }
}

fn register_webview_update_notifications(
    view: &WebView,
    enabled: Arc<AtomicBool>,
) -> windows::core::Result<()> {
    use webview2_com::NewBrowserVersionAvailableEventHandler;

    let handler = NewBrowserVersionAvailableEventHandler::create(Box::new(move |_, _| {
        if enabled.load(Ordering::Relaxed) {
            set_webview_update_status(
                "A new WebView2 runtime has been installed. Restart Zink Connect to use it.".into(),
            );
            tracing::info!("new WebView2 Evergreen runtime available");
        }
        Ok(())
    }));
    let mut token = 0_i64;
    // SAFETY: the handler is a valid WebView2 COM event callback and the token points to
    // writable storage for the duration of this registration call.
    unsafe {
        view.environment()
            .add_NewBrowserVersionAvailable(&handler, &raw mut token)
    }
}

fn webview_update_status() -> &'static std::sync::Mutex<Option<String>> {
    static STATUS: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
        std::sync::OnceLock::new();
    STATUS.get_or_init(|| std::sync::Mutex::new(None))
}

fn set_webview_update_status(message: String) {
    if let Ok(mut status) = webview_update_status().lock() {
        *status = Some(message);
    }
}

fn take_webview_update_status() -> Option<String> {
    webview_update_status().lock().ok()?.take()
}

fn enqueue(queue: &CommandQueue, command: Command) {
    queue.borrow_mut().push_back(command);
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn logical_size(window: &ZinkConnectWindow) -> (u32, u32) {
    let physical = window.window().size();
    let scale = f64::from(window.window().scale_factor());
    (
        (f64::from(physical.width) / scale).round() as u32,
        (f64::from(physical.height) / scale).round() as u32,
    )
}

fn toolbar_bounds(size: (u32, u32)) -> wry::Rect {
    use tao::dpi::{LogicalPosition, LogicalSize};
    wry::Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(f64::from(size.0), CHROME_HEIGHT).into(),
    }
}

fn browser_bounds(size: (u32, u32)) -> wry::Rect {
    use tao::dpi::{LogicalPosition, LogicalSize};
    wry::Rect {
        position: LogicalPosition::new(0.0, CHROME_HEIGHT).into(),
        size: LogicalSize::new(
            f64::from(size.0),
            (f64::from(size.1) - CHROME_HEIGHT).max(0.0),
        )
        .into(),
    }
}

fn fullscreen_bounds(size: (u32, u32)) -> wry::Rect {
    use tao::dpi::{LogicalPosition, LogicalSize};
    wry::Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(f64::from(size.0), f64::from(size.1)).into(),
    }
}

fn set_main_zink_visible(visible: bool) {
    use windows::{
        Win32::UI::WindowsAndMessaging::{FindWindowW, SW_HIDE, SW_RESTORE, ShowWindow},
        core::w,
    };
    // SAFETY: The static title identifies the main Zink top-level window.
    unsafe {
        if let Ok(window) = FindWindowW(None, w!("Zink"))
            && !window.0.is_null()
        {
            let _ = ShowWindow(window, if visible { SW_RESTORE } else { SW_HIDE });
        }
    }
}
