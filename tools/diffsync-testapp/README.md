# diffsync test app

A small Avalonia app (not the product UI, DeltaForge) that proves the C ABI from the real target
UI stack: a custom-drawn text compare control (visible rows only), a folder grid over `ds_view_*`,
a sync dry run, and the question dialog of the sync handshake. Code only (no XAML, no reflection
bindings), so it publishes with NativeAOT.

## Build and run

```powershell
cargo build --release -p core-ffi                 # target\release\diffsync_core.dll
cd tools\diffsync-testapp
dotnet run -c Release                              # the window
dotnet run -c Release -- --selftest                # the engine scenarios without a window
```

## NativeAOT publish

Needs the Visual Studio C++ tools. From a developer command prompt (or after `vcvars64.bat`, with
`vswhere` on the PATH):

```powershell
dotnet publish -c Release -r win-x64 -p:IlcUseEnvironmentalTools=true
bin\x64\Release\net10.0\win-x64\publish\diffsync-testapp.exe --selftest
```

## Self test (automatic)

`--selftest` writes two 1M-line logs, compares them, and replays 4000 frames (smooth scroll, big
jumps, F3 jumps to differences); a frame is what the text control does per draw: the rows, the
line text of both sides and the intraline highlights. It then scans a 20k-node folder tree, pulls
view rows, and runs a sync whose destination was edited after the plan, answering the question from
another thread. The exit code is 0 only when:

* the p99 and the worst frame are within 16 ms (a 60 Hz frame),
* a 100-row pull takes at most 0.2 ms,
* the question arrives on a core thread and is answered from a different one, and the edited
  file is not overwritten.

## Manual test script (window)

1. Start the app. The title shows the ABI version.
2. Type two paths of text files in the boxes (two 1M-line logs from `--selftest` work, or any
   pair) and press **Compare text**. Rows appear while it aligns. Scroll with the wheel, PageUp,
   PageDown, Home, End; press F3 / F4 to jump to the next / previous difference. Scrolling
   stays smooth; the status bar shows the engine time per frame (well under 1 ms).
3. Type two folder paths and press **Compare folders**. The grid fills. Click a folder row to
   expand or collapse it. **Differences only** / **All** change the filter.
4. Press **Plan: mirror to right (dry run)**. The status bar shows the counts; nothing is written.
5. Edit one of the planned files on the right side in another program, then press **Execute
   plan**. A dialog asks what to do with the file that changed since the compare. Choose **Skip**:
   the job finishes and that file is left alone.
6. Close the window: no process stays behind.
