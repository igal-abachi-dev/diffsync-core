// Interop plumbing: blittable-only calls (DisableRuntimeMarshalling), native library resolution,
// the safe handle, errors and strings. NativeAOT compatible: no reflection, no marshalling stubs.
using System;
using System.Collections.Concurrent;
using System.IO;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

[assembly: System.Runtime.CompilerServices.DisableRuntimeMarshalling]

namespace DiffSync.Native;

/// <summary>Status codes of the C ABI (docs/abi.md). Never renumbered.</summary>
public enum DsError
{
    Ok = 0,
    InvalidArg = -1,
    InvalidHandle = -2,
    Cancelled = -3,
    NotFound = -4,
    AccessDenied = -5,
    BufferTooSmall = -6,
    Busy = -7,
    Internal = -8,
    AlreadyExists = -9,
    DiskFull = -10,
    ChangedSinceCompare = -11,
    Unsupported = -12,
    Io = -13,
    Encoding = -14,
    InvalidName = -15,
    PathTooLong = -16,
    SharingViolation = -17,
}

/// <summary>A failed call into the core: the status as <see cref="Kind"/> and the core's message.</summary>
public sealed class DiffSyncException : Exception
{
    public DsError Kind { get; }

    public DiffSyncException(DsError kind, string message) : base(message) => Kind = kind;
}

/// <summary>Owns a core handle and closes it (<c>ds_close</c>) when released. A stale or already
/// closed handle is harmless on the core side, so finalizer-time release is safe.</summary>
public sealed class DsSafeHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public DsSafeHandle() : base(true) { }

    internal DsSafeHandle(ulong handle) : base(true) => SetHandle((nint)handle);

    internal ulong Value => (ulong)handle;

    protected override bool ReleaseHandle()
    {
        // an invalid handle at this point means "already closed": not an error
        _ = NativeMethods.ds_close((ulong)handle);
        return true;
    }
}

internal static unsafe class Native
{
    static Native()
    {
        NativeLibrary.SetDllImportResolver(typeof(Native).Assembly, Resolve);
    }

    /// <summary>Touches the type so the resolver is registered before the first call.</summary>
#pragma warning disable CA2255 // the resolver must exist before the first P/Invoke of a library user
    [ModuleInitializer]
#pragma warning restore CA2255
    internal static void Init() => RuntimeHelpers.RunClassConstructor(typeof(Native).TypeHandle);

    private static nint Resolve(string name, Assembly asm, DllImportSearchPath? path)
    {
        if (name != "diffsync_core") return 0;
        var dir = Environment.GetEnvironmentVariable("DIFFSYNC_DLL_DIR");
        foreach (var candidate in new[] { dir, AppContext.BaseDirectory })
        {
            if (string.IsNullOrEmpty(candidate)) continue;
            var file = Path.Combine(candidate, "diffsync_core.dll");
            if (File.Exists(file) && NativeLibrary.TryLoad(file, out var h)) return h;
        }
        return 0; // the default probing decides
    }

    /// <summary>Throws a <see cref="DiffSyncException"/> for a negative status.</summary>
    internal static void Check(int status)
    {
        if (status >= 0) return;
        throw new DiffSyncException((DsError)status, LastError());
    }

    internal static string LastError()
    {
        nuint needed = 0;
        NativeMethods.ds_last_error(null, 0, &needed);
        if (needed == 0) return "";
        var buf = new byte[(int)needed];
        fixed (byte* p = buf)
        {
            NativeMethods.ds_last_error(p, needed, &needed);
        }
        return Encoding.UTF8.GetString(buf);
    }

    /// <summary>Runs a native call with a JSON argument given as a managed string.</summary>
    internal static DsStr8 Utf8(ReadOnlySpan<byte> bytes, byte* pinned) => new() { ptr = pinned, len = (nuint)bytes.Length };

    internal static byte[] Json(string? s) => string.IsNullOrEmpty(s) ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(s);

    internal static string TakeBuf(DsBuf b)
    {
        try
        {
            return Encoding.UTF8.GetString(b.ptr, (int)b.len);
        }
        finally
        {
            NativeMethods.ds_buf_free(b);
        }
    }
}

/// <summary>Library-wide functions.</summary>
public static unsafe class DiffSyncLibrary
{
    /// <summary>The ABI version, <c>major &lt;&lt; 16 | minor</c>.</summary>
    public static uint AbiVersion => NativeMethods.ds_abi_version();

    public static int AbiMajor => (int)(AbiVersion >> 16);

    public static int AbiMinor => (int)(AbiVersion & 0xFFFF);

    /// <summary>The JSON Schema of every option set.</summary>
    public static string OptionsSchema()
    {
        DsBuf b = default;
        Native.Check(NativeMethods.ds_options_schema(&b));
        return Native.TakeBuf(b);
    }

    /// <summary>Optional one-time setup: the folder for execution logs and the worker thread count.</summary>
    public static void Init(string? logDir = null, uint workerThreads = 0)
    {
        var dir = logDir ?? "";
        fixed (char* p = dir)
        {
            var o = new DsInitOptions
            {
                struct_size = (uint)sizeof(DsInitOptions),
                worker_threads = workerThreads,
                log_dir = new DsStr16 { ptr = (ushort*)p, len = (nuint)dir.Length },
            };
            Native.Check(NativeMethods.ds_init(&o));
        }
    }
}

/// <summary>One event from the core (docs/abi.md, "Events").</summary>
public readonly record struct DsEvent(ulong Source, EventKind Kind, ulong A, ulong B);

public enum EventKind : uint { State = 1, Rows = 2, Progress = 3, Node = 4, Question = 5, Log = 6 }

/// <summary>
/// Turns the core's callback into managed events. The native <c>user</c> pointer is an id, not a
/// GCHandle: a callback that races with <c>Dispose</c> finds nothing and returns, instead of
/// touching freed memory. Exceptions never cross back into native code.
/// </summary>
internal sealed unsafe class EventPump : IDisposable
{
    private static readonly ConcurrentDictionary<nint, EventPump> Pumps = new();
    private static long _next;

    private readonly nint _id;
    private readonly Action<DsEvent> _handler;
    private readonly Action<Action>? _post;

    private EventPump(Action<DsEvent> handler, Action<Action>? post)
    {
        _id = (nint)Interlocked.Increment(ref _next);
        _handler = handler;
        _post = post;
        Pumps[_id] = this;
    }

    /// <summary>Null when there is no handler (then no callback is registered at all).</summary>
    internal static EventPump? Create(Action<DsEvent>? handler, Action<Action>? post) =>
        handler is null ? null : new EventPump(handler, post);

    internal void* User => (void*)_id;

    internal static delegate* unmanaged[Cdecl]<void*, ulong, uint, ulong, ulong, void> Callback => &OnEvent;

    [UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]
    private static void OnEvent(void* user, ulong src, uint kind, ulong a, ulong b)
    {
        try
        {
            if (!Pumps.TryGetValue((nint)user, out var pump)) return;
            var e = new DsEvent(src, (EventKind)kind, a, b);
            if (pump._post is { } post) post(() => pump._handler(e));
            else pump._handler(e);
        }
        catch
        {
            // a handler that throws must not take the core's thread down
        }
    }

    public void Dispose() => Pumps.TryRemove(_id, out _);
}
