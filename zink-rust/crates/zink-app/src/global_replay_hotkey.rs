use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use slint::Weak;
use tracing::{error, info};
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey},
    WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY},
};

use crate::AppWindow;

const SAVE_REPLAY_HOTKEY_ID: i32 = 5_001;
const VK_Z: u32 = 0x5A;

/// Registers the same native Ctrl+Z replay shortcut used by the `WinUI` Zink app.
///
/// Registration lives on its own Windows message-loop thread so the shortcut remains
/// global while Slint owns the main window loop. The preference is checked again when
/// Windows delivers the shortcut, allowing Settings to enable or disable it immediately.
pub fn start(ui: Weak<AppWindow>, enabled: Arc<AtomicBool>) {
    if let Err(error) = std::thread::Builder::new()
        .name("zink-replay-hotkey".to_owned())
        .spawn(move || unsafe {
            if let Err(error) = RegisterHotKey(
                None,
                SAVE_REPLAY_HOTKEY_ID,
                MOD_CONTROL | MOD_NOREPEAT,
                VK_Z,
            ) {
                error!(%error, "could not register the global Ctrl+Z replay shortcut");
                return;
            }

            info!("global Ctrl+Z replay shortcut registered");
            let mut message = MSG::default();
            while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
                if message.message == WM_HOTKEY
                    && message.wParam.0 == SAVE_REPLAY_HOTKEY_ID as usize
                    && enabled.load(Ordering::Relaxed)
                {
                    let ui = ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui.upgrade() {
                            ui.invoke_recorder_command("save-replay".into());
                        }
                    });
                }
            }

            let _ = UnregisterHotKey(None, SAVE_REPLAY_HOTKEY_ID);
        })
    {
        error!(%error, "could not start the replay hotkey thread");
    }
}
