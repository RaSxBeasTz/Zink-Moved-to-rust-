#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsRuntime;

impl WindowsRuntime {
    /// Establish process-level Windows defaults before Slint creates HWNDs.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows process initialization fails.
    pub fn initialize() -> Result<Self, PlatformError> {
        initialize_platform()?;
        Ok(Self)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("Windows API initialization failed: {0}")]
    Windows(String),
    #[error("Zink's native application is supported only on Windows")]
    UnsupportedPlatform,
}

/// Creates or updates the Start-menu shortcut used to launch Zink Connect directly.
/// Windows then exposes the shortcut in Start so the user can pin it to the taskbar.
///
/// # Errors
///
/// Returns an error if the shortcut cannot be created or saved.
#[cfg(windows)]
pub fn create_zink_connect_shortcut() -> Result<std::path::PathBuf, PlatformError> {
    use windows::{
        Win32::{
            Foundation::PROPERTYKEY,
            System::Com::StructuredStorage::PROPVARIANT,
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile},
            UI::Shell::{
                IShellLinkW, PropertiesSystem::IPropertyStore,
                SetCurrentProcessExplicitAppUserModelID, ShellLink,
            },
        },
        core::{HSTRING, Interface},
    };

    const ZINK_CONNECT_APP_ID: &str = "Zink.Connect";
    const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };

    let executable =
        std::env::current_exe().map_err(|error| PlatformError::Windows(error.to_string()))?;
    let programs = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| PlatformError::Windows("APPDATA is unavailable".into()))?
        .join("Microsoft/Windows/Start Menu/Programs");
    std::fs::create_dir_all(&programs)
        .map_err(|error| PlatformError::Windows(error.to_string()))?;
    let shortcut = programs.join("Zink Connect.lnk");

    // SAFETY: COM is initialized by WindowsRuntime. All strings live through the calls.
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(windows::core::w!("Zink.Connect"))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let executable_text = HSTRING::from(executable.to_string_lossy().as_ref());
        link.SetPath(&executable_text)
            .and_then(|()| link.SetArguments(windows::core::w!("--zink-connect")))
            .and_then(|()| link.SetDescription(windows::core::w!("Open the Zink Connect browser")))
            .and_then(|()| link.SetIconLocation(&executable_text, 0))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let property_store: IPropertyStore = link
            .cast()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let app_id = PROPVARIANT::from(ZINK_CONNECT_APP_ID);
        property_store
            .SetValue(&PKEY_APP_USER_MODEL_ID, &raw const app_id)
            .and_then(|()| property_store.Commit())
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let persist: IPersistFile = link
            .cast()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let shortcut_text = HSTRING::from(shortcut.to_string_lossy().as_ref());
        persist
            .Save(&shortcut_text, true)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
    }
    Ok(shortcut)
}

/// Opens Windows' native confirmation prompt for pinning the foreground app.
/// Windows deliberately requires the user to approve this request.
///
/// # Errors
///
/// Returns an error when taskbar pinning is unavailable or prohibited by policy.
#[cfg(windows)]
pub fn request_taskbar_pin() -> Result<(), PlatformError> {
    use windows::UI::Shell::TaskbarManager;
    use windows_future::AsyncOperationCompletedHandler;

    let manager =
        TaskbarManager::GetDefault().map_err(|error| PlatformError::Windows(error.to_string()))?;
    if !manager
        .IsSupported()
        .map_err(|error| PlatformError::Windows(error.to_string()))?
    {
        return Err(PlatformError::Windows(
            "Taskbar pinning is not supported on this device".into(),
        ));
    }
    if !manager
        .IsPinningAllowed()
        .map_err(|error| PlatformError::Windows(error.to_string()))?
    {
        return Err(PlatformError::Windows(
            "Taskbar pinning is disabled by Windows or system policy".into(),
        ));
    }

    let request = manager
        .RequestPinCurrentAppAsync()
        .map_err(|error| PlatformError::Windows(error.to_string()))?;
    request
        .SetCompleted(&AsyncOperationCompletedHandler::new(|operation, _| {
            let _ = operation.ok()?.GetResults()?;
            Ok(())
        }))
        .map_err(|error| PlatformError::Windows(error.to_string()))?;
    Ok(())
}

#[cfg(not(windows))]
pub fn request_taskbar_pin() -> Result<(), PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn create_zink_connect_shortcut() -> Result<std::path::PathBuf, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

/// Keeps the named Windows mutex alive for the lifetime of the primary Zink process.
#[cfg(windows)]
#[derive(Debug)]
pub struct SingleInstance(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: This handle was returned by CreateMutexW and is owned by this guard.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Claims Zink's process-wide single-instance mutex. If another instance owns it,
/// that instance's main window is restored and focused and `None` is returned.
///
/// # Errors
///
/// Returns an error if the mutex cannot be created.
#[cfg(windows)]
pub fn acquire_single_instance() -> Result<Option<SingleInstance>, PlatformError> {
    use windows::{
        Win32::{
            Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::CreateMutexW,
        },
        core::w,
    };

    // SAFETY: The mutex has a static, process-independent name and default security.
    unsafe {
        let mutex = CreateMutexW(None, false, w!("Local\\Zink.App.SingleInstance"))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = windows::Win32::Foundation::CloseHandle(mutex);
            activate_zink_window();
            Ok(None)
        } else {
            Ok(Some(SingleInstance(mutex)))
        }
    }
}

#[cfg(windows)]
fn activate_zink_window() {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
        },
        core::w,
    };

    // SAFETY: The static title identifies Zink's primary top-level window.
    unsafe {
        let window =
            FindWindowW(None, w!("Zink Connect")).or_else(|_| FindWindowW(None, w!("Zink")));
        if let Ok(window) = window
            && !window.0.is_null()
        {
            let _ = ShowWindow(window, SW_RESTORE);
            let _ = SetForegroundWindow(window);
        }
    }
}

#[cfg(not(windows))]
#[derive(Debug)]
pub struct SingleInstance;

#[cfg(not(windows))]
pub fn acquire_single_instance() -> Result<Option<SingleInstance>, PlatformError> {
    Ok(Some(SingleInstance))
}

/// Opens the native Zink Connect Chromium window on its own Windows UI thread.
/// Repeated calls keep a single browser window instead of creating duplicates.
///
/// # Errors
///
/// Returns an error when the browser thread cannot be created.
#[cfg(windows)]
pub fn open_zink_connect() -> Result<(), PlatformError> {
    static BROWSER_PROCESS: std::sync::Mutex<Option<std::process::Child>> =
        std::sync::Mutex::new(None);

    let mut process = BROWSER_PROCESS
        .lock()
        .map_err(|error| PlatformError::Windows(error.to_string()))?;
    if let Some(child) = process.as_mut()
        && child
            .try_wait()
            .map_err(|error| PlatformError::Windows(error.to_string()))?
            .is_none()
    {
        activate_zink_connect_window()?;
        return Ok(());
    }

    let browser_path = std::env::current_exe()
        .map_err(|error| PlatformError::Windows(error.to_string()))?
        .with_file_name("zink-connect.exe");
    let owner = unsafe {
        use windows::{Win32::UI::WindowsAndMessaging::FindWindowW, core::w};
        FindWindowW(None, w!("Zink"))
            .ok()
            .map(|window| window.0 as usize)
    };
    let mut command = std::process::Command::new(&browser_path);
    if let Some(owner) = owner {
        command.arg("--owner-hwnd").arg(owner.to_string());
    }
    let child = command.spawn().map_err(|error| {
        PlatformError::Windows(format!(
            "failed to start {}: {error}",
            browser_path.display()
        ))
    })?;
    *process = Some(child);
    Ok(())
}

#[cfg(windows)]
fn activate_zink_connect_window() -> Result<(), PlatformError> {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
        },
        core::w,
    };

    // SAFETY: The title is a static UTF-16 string. The returned HWND is used
    // only for standard visibility and foreground activation calls.
    unsafe {
        let window = FindWindowW(None, w!("Zink Connect"))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if window.0.is_null() {
            return Ok(());
        }
        let _ = ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window)
            .ok()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn open_zink_connect() -> Result<(), PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

/// Shows the native Windows file-open dialog and returns a selected media file.
/// Cancellation is not an error.
///
/// # Errors
///
/// Returns an error if COM or the Windows shell dialog cannot be initialized.
#[cfg(windows)]
pub fn pick_media_file() -> Result<Option<std::path::PathBuf>, PlatformError> {
    use windows::{
        Win32::{
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree,
            },
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FileOpenDialog,
                IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::w,
    };

    // SAFETY: The dialog is created in the caller's STA. COM owns the dialog
    // interfaces, and the returned path buffer is released with CoTaskMemFree.
    unsafe {
        let initialization = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if initialization.is_err() && initialization.0 != 0x8001_0106_u32.cast_signed() {
            return Err(PlatformError::Windows(format!(
                "COM initialization failed: {initialization:?}"
            )));
        }

        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetFileTypes(&[
                COMDLG_FILTERSPEC {
                    pszName: w!("Supported media"),
                    pszSpec: w!(
                        "*.mp3;*.m4a;*.aac;*.wav;*.flac;*.wma;*.mp4;*.mkv;*.mov;*.avi;*.webm"
                    ),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("All files"),
                    pszSpec: w!("*.*"),
                },
            ])
            .map_err(|error| PlatformError::Windows(error.to_string()))?;

        if let Err(error) = dialog.Show(None) {
            // HRESULT_FROM_WIN32(ERROR_CANCELLED)
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(None);
            }
            return Err(PlatformError::Windows(error.to_string()));
        }

        let item = dialog
            .GetResult()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let raw_path = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let path = raw_path
            .to_string()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        CoTaskMemFree(Some(raw_path.0.cast()));
        Ok(Some(path.into()))
    }
}

/// Shows the native Windows file-open dialog for an SRT or `WebVTT` subtitle file.
/// Cancellation is not an error.
///
/// # Errors
///
/// Returns an error if COM or the Windows shell dialog cannot be initialized.
#[cfg(windows)]
pub fn pick_subtitle_file() -> Result<Option<std::path::PathBuf>, PlatformError> {
    use windows::{
        Win32::{
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FileOpenDialog,
                IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::w,
    };

    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .and_then(|()| {
                dialog.SetFileTypes(&[
                    COMDLG_FILTERSPEC {
                        pszName: w!("Subtitle files"),
                        pszSpec: w!("*.srt;*.vtt"),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: w!("All files"),
                        pszSpec: w!("*.*"),
                    },
                ])
            })
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if let Err(error) = dialog.Show(None) {
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(None);
            }
            return Err(PlatformError::Windows(error.to_string()));
        }
        let item = dialog
            .GetResult()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let raw_path = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let path = raw_path
            .to_string()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        CoTaskMemFree(Some(raw_path.0.cast()));
        Ok(Some(path.into()))
    }
}

/// Shows the native Windows multi-select file dialog for building a music queue.
/// Cancellation returns an empty list.
///
/// # Errors
///
/// Returns an error if COM or the Windows shell dialog cannot be initialized.
#[cfg(windows)]
pub fn pick_music_files() -> Result<Vec<std::path::PathBuf>, PlatformError> {
    use windows::{
        Win32::{
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
                FOS_FORCEFILESYSTEM, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::w,
    };

    // SAFETY: COM is initialized by WindowsRuntime. Shell interfaces own their
    // results and each returned file-system path is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .and_then(|()| {
                dialog.SetFileTypes(&[
                    COMDLG_FILTERSPEC {
                        pszName: w!("Music files"),
                        pszSpec: w!("*.mp3;*.mp4;*.m4a;*.aac;*.wav;*.flac;*.wma"),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: w!("All files"),
                        pszSpec: w!("*.*"),
                    },
                ])
            })
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if let Err(error) = dialog.Show(None) {
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(Vec::new());
            }
            return Err(PlatformError::Windows(error.to_string()));
        }
        let results = dialog
            .GetResults()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let count = results
            .GetCount()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let raw_path = results
                .GetItemAt(index)
                .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
                .map_err(|error| PlatformError::Windows(error.to_string()))?;
            let path = raw_path
                .to_string()
                .map_err(|error| PlatformError::Windows(error.to_string()))?;
            CoTaskMemFree(Some(raw_path.0.cast()));
            paths.push(path.into());
        }
        Ok(paths)
    }
}

/// Shows a native Windows multi-select dialog for files handled by Zink's file mover.
/// Cancellation returns an empty list.
///
/// # Errors
///
/// Returns an error if COM or the Windows shell dialog cannot be initialized.
#[cfg(windows)]
pub fn pick_files_to_move() -> Result<Vec<std::path::PathBuf>, PlatformError> {
    use windows::{
        Win32::{
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
                FOS_FORCEFILESYSTEM, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::w,
    };

    // SAFETY: COM is initialized by WindowsRuntime. Shell interfaces own their
    // results and each returned file-system path is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .and_then(|()| {
                dialog.SetFileTypes(&[
                    COMDLG_FILTERSPEC {
                        pszName: w!("Media and archive files"),
                        pszSpec: w!("*.mp3;*.m4a;*.aac;*.wav;*.flac;*.wma;*.mp4;*.m4v;*.mkv;*.mov;*.avi;*.wmv;*.webm;*.ts;*.m2ts;*.zip;*.7z"),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: w!("All files"),
                        pszSpec: w!("*.*"),
                    },
                ])
            })
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if let Err(error) = dialog.Show(None) {
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(Vec::new());
            }
            return Err(PlatformError::Windows(error.to_string()));
        }
        let results = dialog
            .GetResults()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let count = results
            .GetCount()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let raw_path = results
                .GetItemAt(index)
                .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
                .map_err(|error| PlatformError::Windows(error.to_string()))?;
            let path = raw_path
                .to_string()
                .map_err(|error| PlatformError::Windows(error.to_string()))?;
            CoTaskMemFree(Some(raw_path.0.cast()));
            paths.push(path.into());
        }
        Ok(paths)
    }
}

/// Shows the native Windows file dialog for selecting a profile image.
/// Cancellation returns `None`.
///
/// # Errors
///
/// Returns an error if the Windows shell dialog cannot be created or queried.
#[cfg(windows)]
pub fn pick_image_file() -> Result<Option<std::path::PathBuf>, PlatformError> {
    use windows::{
        Win32::{
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FileOpenDialog,
                IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::w,
    };

    // SAFETY: the process initializes COM before UI setup. The shell owns the
    // result item and its allocated path is released with `CoTaskMemFree`.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .and_then(|()| {
                dialog.SetFileTypes(&[
                    COMDLG_FILTERSPEC {
                        pszName: w!("Image files"),
                        pszSpec: w!("*.jpg;*.jpeg;*.png;*.gif;*.webp"),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: w!("All files"),
                        pszSpec: w!("*.*"),
                    },
                ])
            })
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if let Err(error) = dialog.Show(None) {
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(None);
            }
            return Err(PlatformError::Windows(error.to_string()));
        }
        let raw_path = dialog
            .GetResult()
            .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let path = raw_path
            .to_string()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        CoTaskMemFree(Some(raw_path.0.cast()));
        Ok(Some(path.into()))
    }
}

/// Shows the native Windows folder picker for importing a media library.
/// Cancellation is not an error.
///
/// # Errors
///
/// Returns an error if COM or the Windows shell dialog cannot be initialized.
#[cfg(windows)]
pub fn pick_media_folder() -> Result<Option<std::path::PathBuf>, PlatformError> {
    use windows::Win32::{
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
        UI::Shell::{
            FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog,
            IFileOpenDialog, SIGDN_FILESYSPATH,
        },
    };

    // SAFETY: The process is already initialized as an STA. COM owns the
    // dialog interfaces, and the returned path is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(FOS_PICKFOLDERS | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if let Err(error) = dialog.Show(None) {
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(None);
            }
            return Err(PlatformError::Windows(error.to_string()));
        }

        let raw_path = dialog
            .GetResult()
            .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let path = raw_path
            .to_string()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        CoTaskMemFree(Some(raw_path.0.cast()));
        Ok(Some(path.into()))
    }
}

/// Shows the native Windows multi-select folder picker for importing several
/// media roots in one scan. Cancellation returns an empty list.
///
/// # Errors
///
/// Returns an error if COM or the Windows shell dialog cannot be initialized.
#[cfg(windows)]
pub fn pick_media_folders() -> Result<Vec<std::path::PathBuf>, PlatformError> {
    use windows::Win32::{
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
        UI::Shell::{
            FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
            FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        },
    };

    // SAFETY: COM is initialized by WindowsRuntime. Shell interfaces own their
    // results and each returned file-system path is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        dialog
            .SetOptions(
                FOS_ALLOWMULTISELECT | FOS_PICKFOLDERS | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM,
            )
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        if let Err(error) = dialog.Show(None) {
            if error.code().0 == 0x8007_04C7_u32.cast_signed() {
                return Ok(Vec::new());
            }
            return Err(PlatformError::Windows(error.to_string()));
        }
        let results = dialog
            .GetResults()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let count = results
            .GetCount()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        let mut paths = Vec::with_capacity(count.min(4) as usize);
        for index in 0..count.min(4) {
            let raw_path = results
                .GetItemAt(index)
                .and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
                .map_err(|error| PlatformError::Windows(error.to_string()))?;
            let path = raw_path
                .to_string()
                .map_err(|error| PlatformError::Windows(error.to_string()))?;
            CoTaskMemFree(Some(raw_path.0.cast()));
            paths.push(path.into());
        }
        Ok(paths)
    }
}

#[cfg(not(windows))]
pub fn pick_media_file() -> Result<Option<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn pick_subtitle_file() -> Result<Option<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn pick_music_files() -> Result<Vec<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn pick_files_to_move() -> Result<Vec<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn pick_image_file() -> Result<Option<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn pick_media_folder() -> Result<Option<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn pick_media_folders() -> Result<Vec<std::path::PathBuf>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}

#[cfg(windows)]
fn initialize_platform() -> Result<(), PlatformError> {
    use windows::Win32::{
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Shell::SetCurrentProcessExplicitAppUserModelID,
        },
    };

    // SAFETY: These establish process/thread state before UI or COM objects are
    // created. The COM apartment remains active for the process lifetime.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
        SetCurrentProcessExplicitAppUserModelID(windows::core::w!("Zink.App"))
            .map_err(|error| PlatformError::Windows(error.to_string()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn initialize_platform() -> Result<(), PlatformError> {
    Err(PlatformError::UnsupportedPlatform)
}
