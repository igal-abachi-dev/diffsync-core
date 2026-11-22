//! The COM objects, driven the way Explorer drives them (backlog E11.5-S7.1): through the DLL's own
//! `DllGetClassObject`, a class factory, and `IExplorerCommand` with real `IShellItemArray`s of real
//! files. One test in a process: it sets environment variables (the selection folder, the compare
//! tool, silent message boxes).
#![cfg(windows)]
#![allow(unsafe_code, clippy::borrow_as_ptr)]

use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

use diffsync_shell::{CLSID_COMPARE_WITH, CLSID_SELECT_LEFT, DllCanUnloadNow, DllGetClassObject};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, S_OK};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IClassFactory,
};
use windows::Win32::UI::Shell::{
    ECS_ENABLED, ECS_HIDDEN, IExplorerCommand, IShellItem, IShellItemArray,
    SHCreateItemFromParsingName, SHCreateShellItemArrayFromShellItem,
};
use windows::core::{GUID, HSTRING, Interface};

fn command(clsid: &GUID) -> IExplorerCommand {
    let mut factory: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid pointers; the factory interface is taken over by `from_raw`.
    unsafe {
        DllGetClassObject(clsid, &IClassFactory::IID, &raw mut factory)
            .ok()
            .unwrap();
        let factory = IClassFactory::from_raw(factory);
        factory.CreateInstance::<_, IExplorerCommand>(None).unwrap()
    }
}

fn items(path: &Path) -> IShellItemArray {
    // SAFETY: plain shell calls with a valid string.
    unsafe {
        let item: IShellItem =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).unwrap();
        SHCreateShellItemArrayFromShellItem(&item).unwrap()
    }
}

fn title(c: &IExplorerCommand, a: &IShellItemArray) -> String {
    // SAFETY: the returned string is allocated for the caller and freed once.
    unsafe {
        let p = c.GetTitle(a).unwrap();
        let s = p.to_string().unwrap();
        CoTaskMemFree(Some(p.0.cast()));
        s
    }
}

fn state(c: &IExplorerCommand, a: &IShellItemArray) -> i32 {
    // SAFETY: a plain COM call.
    unsafe { c.GetState(a, false).unwrap() as i32 }
}

#[test]
fn select_left_then_compare_through_the_com_objects() {
    let d = tempfile::tempdir().unwrap();
    let state_dir = d.path().join("state");
    let (f1, f2) = (d.path().join("one.txt"), d.path().join("two.txt"));
    std::fs::write(&f1, "1").unwrap();
    std::fs::write(&f2, "2").unwrap();
    let out = d.path().join("launched.txt");
    // SAFETY: set before the objects are created; this binary has one test.
    unsafe {
        std::env::set_var("DIFFSYNC_STATE_DIR", &state_dir);
        std::env::set_var("DIFFSYNC_SHELL_SILENT", "1");
        // the "compare tool": cmd writes the two paths it was given into a file
        std::env::set_var(
            "DIFFSYNC_SHELL_COMMAND",
            format!("cmd.exe /c \"echo %L;%R>{}\"", out.display()),
        );
    }
    // SAFETY: balanced with CoUninitialize below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };

    // an unknown class
    let mut obj: *mut c_void = std::ptr::null_mut();
    let unknown = GUID::from_u128(0x1234);
    // SAFETY: valid pointers.
    assert_eq!(
        unsafe { DllGetClassObject(&raw const unknown, &IClassFactory::IID, &raw mut obj,) },
        CLASS_E_CLASSNOTAVAILABLE
    );

    let (select, compare) = (command(&CLSID_SELECT_LEFT), command(&CLSID_COMPARE_WITH));
    let (a1, a2) = (items(&f1), items(&f2));
    // before anything is selected only "Select left side" is offered
    assert_eq!(title(&select, &a1), "Select left side");
    assert_eq!(state(&select, &a1), ECS_ENABLED.0);
    assert_eq!(state(&compare, &a2), ECS_HIDDEN.0);
    // SAFETY: plain COM calls.
    unsafe {
        assert_eq!(select.GetCanonicalName().unwrap(), CLSID_SELECT_LEFT);
        select.Invoke(&a1, None).unwrap();
    }
    // now the second file can be compared with it, the first cannot be compared with itself
    assert_eq!(state(&compare, &a2), ECS_ENABLED.0);
    assert_eq!(state(&compare, &a1), ECS_HIDDEN.0);
    assert_eq!(title(&compare, &a2), "Compare with \"one.txt\"");
    // SAFETY: a plain COM call.
    unsafe { compare.Invoke(&a2, None).unwrap() };
    let t = Instant::now();
    let launched = loop {
        if let Ok(s) = std::fs::read_to_string(&out) {
            if s.contains(';') {
                break s;
            }
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the compare tool was not started"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        launched.contains(f1.to_str().unwrap()) && launched.contains(f2.to_str().unwrap()),
        "{launched}"
    );

    // the objects are released: the DLL may unload once they are dropped
    drop((select, compare));
    // a safe function (it only reads a counter)
    assert_eq!(DllCanUnloadNow(), S_OK);
    // SAFETY: balanced with the init above.
    unsafe { CoUninitialize() };
}
