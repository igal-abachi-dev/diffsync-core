//! The COM objects: the two commands and their class factory.
// the `#[implement]` macro of windows-rs generates code these pedantic lints dislike
#![allow(clippy::ref_as_ptr, clippy::inline_always, clippy::borrow_as_ptr)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use core_tools::SelectionStore;
use core_tools::shell::{self, LaunchConfig, ShellAction, ShellCommand, ShellState};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOTIMPL, E_POINTER, HMODULE,
    S_FALSE, S_OK,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::UI::Shell::{
    ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand, IExplorerCommand,
    IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, PWSTR, Ref, Result, implement, w};

pub const CLSID_SELECT_LEFT: GUID = GUID::from_u128(0xB3F2A1C0_5D7E_4C8A_9E21_0A6D3F4B7C11);
pub const CLSID_COMPARE_WITH: GUID = GUID::from_u128(0xB3F2A1C0_5D7E_4C8A_9E21_0A6D3F4B7C12);

/// Live COM objects and `LockServer` locks: `DllCanUnloadNow` says "yes" at zero.
static LIVE: AtomicI32 = AtomicI32::new(0);

#[implement(IExplorerCommand)]
struct Command {
    which: ShellCommand,
}

impl Command {
    fn new(which: ShellCommand) -> Self {
        LIVE.fetch_add(1, Ordering::SeqCst);
        Command { which }
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn store() -> Option<SelectionStore> {
    SelectionStore::for_current_user()
}

/// The paths of the items Explorer passes (none when it passes none).
fn paths_of(items: Option<&IShellItemArray>) -> Vec<PathBuf> {
    let Some(items) = items else {
        return Vec::new();
    };
    // SAFETY: plain COM calls on a live interface; every returned string is freed once.
    unsafe {
        let n = items.GetCount().unwrap_or(0);
        (0..n)
            .filter_map(|i| {
                let item = items.GetItemAt(i).ok()?;
                let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
                let s = name.to_string().ok();
                CoTaskMemFree(Some(name.0.cast()));
                s.map(PathBuf::from)
            })
            .collect()
    }
}

/// The command line template: the environment, then the registry; the folder of this DLL is where
/// the default tool is looked for.
fn launch_config() -> LaunchConfig {
    let template = std::env::var("DIFFSYNC_SHELL_COMMAND")
        .ok()
        .or_else(registry_template);
    LaunchConfig {
        template,
        module_dir: module_dir(),
    }
}

fn registry_template() -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its size in bytes are valid; the key and value names are literals.
    let st = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\DiffSync\\Shell"),
            w!("Command"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut bytes),
        )
    };
    if st.is_err() {
        return None;
    }
    let units = (bytes as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..units]))
}

fn module_dir() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    // SAFETY: the address of a function of this module identifies it; no reference is added.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(module_dir as *const u16),
            &raw mut module,
        )
        .ok()?;
        let mut buf = vec![0u16; 1024];
        let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        path.parent().map(PathBuf::from)
    }
}

fn warn(msg: &str) {
    // tests (and unattended installs) set this: a dialog would wait for a click
    if std::env::var_os("DIFFSYNC_SHELL_SILENT").is_some() {
        return;
    }
    let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            w!("DiffSync"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

#[allow(non_snake_case)]
impl IExplorerCommand_Impl for Command_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        let text = match store() {
            Some(s) => shell::title(self.which, &s),
            None => shell::title(
                self.which,
                &SelectionStore::in_dir(std::path::Path::new(".")),
            ),
        };
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated; SHStrDupW allocates with CoTaskMemAlloc for Explorer to free.
        unsafe { SHStrDupW(windows::core::PCWSTR(wide.as_ptr())) }
    }

    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(match self.which {
            ShellCommand::SelectLeft => CLSID_SELECT_LEFT,
            ShellCommand::CompareWithSelected => CLSID_COMPARE_WITH,
        })
    }

    fn GetState(&self, items: Ref<'_, IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        let paths = paths_of(items.as_ref());
        let st = store().map_or(ShellState::Hidden, |s| shell::state(self.which, &paths, &s));
        Ok(if st == ShellState::Enabled {
            ECS_ENABLED.0 as u32
        } else {
            ECS_HIDDEN.0 as u32
        })
    }

    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _bind_ctx: Ref<'_, IBindCtx>) -> Result<()> {
        let paths = paths_of(items.as_ref());
        let Some(store) = store() else {
            warn("DiffSync: there is no local data folder to keep the selection in.");
            return Err(E_FAIL.into());
        };
        match shell::invoke(self.which, &paths, &store, &launch_config()) {
            ShellAction::Remembered(_) => Ok(()),
            ShellAction::Launch { program, args } => {
                match std::process::Command::new(&program).args(&args).spawn() {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        warn(&format!(
                            "DiffSync: cannot start {}: {e}",
                            program.display()
                        ));
                        Err(E_FAIL.into())
                    }
                }
            }
            ShellAction::Failed(m) => {
                warn(&format!("DiffSync: {m}"));
                Err(E_FAIL.into())
            }
        }
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct Factory {
    which: ShellCommand,
}

#[allow(non_snake_case)]
impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> Result<()> {
        if object.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: both pointers were checked; the caller owns the out slot.
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let command: IExplorerCommand = Command::new(self.which).into();
        // SAFETY: `iid` and `object` are valid for the query; the reference count is handled by COM.
        unsafe { command.query(iid, object).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        if lock.as_bool() {
            LIVE.fetch_add(1, Ordering::SeqCst);
        } else {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// COM entry point: hands out the class factory of one of the two commands.
///
/// # Safety
/// Called by COM with valid pointers (checked for null here).
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    object: *mut *mut c_void,
) -> HRESULT {
    if clsid.is_null() || iid.is_null() || object.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked above.
    let (clsid, iid) = unsafe { (*clsid, *iid) };
    let which = if clsid == CLSID_SELECT_LEFT {
        ShellCommand::SelectLeft
    } else if clsid == CLSID_COMPARE_WITH {
        ShellCommand::CompareWithSelected
    } else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    let factory: IClassFactory = Factory { which }.into();
    // SAFETY: `object` was checked; `query` writes an interface pointer or an error.
    unsafe { factory.query(&raw const iid, object) }
}

/// COM entry point: whether the DLL may be unloaded.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::SeqCst) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}
