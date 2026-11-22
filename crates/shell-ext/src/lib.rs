//! `diffsync_shell.dll`: the Windows Explorer context-menu handler (backlog E11.5-S7).
//!
//! Two `IExplorerCommand` objects, like Beyond Compare's "Select left side" and "Compare to":
//!
//! | CLSID | Entry |
//! |---|---|
//! | `{B3F2A1C0-5D7E-4C8A-9E21-0A6D3F4B7C11}` | Select left side |
//! | `{B3F2A1C0-5D7E-4C8A-9E21-0A6D3F4B7C12}` | Compare with "<left>" |
//!
//! All decisions (when an entry is shown, its title, what a click does, how the compare tool is
//! started) live in `core_tools::shell` and are tested without Explorer; this crate translates
//! Explorer's calls into them. The handler works in the classic menu (registered per user under
//! `HKCU`, no administrator rights: `scripts/shell/register-shell.ps1`) and, wrapped in a sparse
//! package (`shell/AppxManifest.xml`), in the Windows 11 top-level menu.
//!
//! Settings, read when an entry is used: the environment variable `DIFFSYNC_SHELL_COMMAND` or the
//! registry value `HKCU\Software\DiffSync\Shell\Command` is the command line template (`%L`, `%R`);
//! without either, `DeltaForge.exe` next to this DLL is started with the two paths.
//!
//! A shell extension runs inside Explorer: nothing here may panic or block, and a failure shows a
//! message box instead of crashing the shell.
#![allow(unsafe_code)]
// the two COM exports are exported by name on purpose; MSVC notes that they could be PRIVATE
#![allow(linker_messages)]

#[cfg(windows)]
mod com;

#[cfg(windows)]
pub use com::{CLSID_COMPARE_WITH, CLSID_SELECT_LEFT, DllCanUnloadNow, DllGetClassObject};

/// The CLSIDs as text with braces, for the registration scripts and the manifest.
pub const CLSID_SELECT_LEFT_TEXT: &str = "{B3F2A1C0-5D7E-4C8A-9E21-0A6D3F4B7C11}";
pub const CLSID_COMPARE_WITH_TEXT: &str = "{B3F2A1C0-5D7E-4C8A-9E21-0A6D3F4B7C12}";
