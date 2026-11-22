// Entry point. `--selftest` runs the engine scenarios without a window (1M-row scroll timing,
// folder grid, sync question); `--window-smoke` opens the real window, draws a compare and closes
// itself; no argument starts the app.
using System;
using Avalonia;

namespace DiffSync.TestApp;

internal static class Program
{
    [STAThread]
    public static int Main(string[] args)
    {
        if (Array.IndexOf(args, "--selftest") >= 0) return SelfTest.Run(args);
        return BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    }

    public static AppBuilder BuildAvaloniaApp() => AppBuilder.Configure<App>().UsePlatformDetect();
}
