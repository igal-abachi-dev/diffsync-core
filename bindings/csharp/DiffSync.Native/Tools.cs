// Integration helpers of ABI 1.1 (backlog E11.5-S5.2): diff reports, how other programs start the
// tool, ignore lists, the Explorer selection, the three-way merge stub, and free disk space.
using System;
using System.Text;

namespace DiffSync.Native;

/// <summary>
/// What a UI, a command-line wrapper or an MCP server needs to behave like <c>diff -u</c>,
/// <c>git diff --no-index</c> and a source-control merge/compare tool. All JSON shapes are in
/// <c>docs/abi.md</c>. The calls are synchronous: run them off the UI thread.
/// </summary>
public static unsafe class Tools
{
    private static string Call(delegate*<DsStr8, DsBuf*, int> f, string json)
    {
        var bytes = Encoding.UTF8.GetBytes(json);
        DsBuf b = default;
        fixed (byte* p = bytes)
        {
            Native.Check(f(new DsStr8 { ptr = p, len = (nuint)bytes.Length }, &b));
        }
        return Native.TakeBuf(b);
    }

    /// <summary>Runs a <c>diff</c> / <c>git diff</c> command line: returns the JSON report
    /// <c>{stdout, stderr, exitCode, json, ignored}</c>. Request: <c>{"args":["-ru","a","b"],"dialect":"gnu"}</c>.</summary>
    public static string DiffRun(string requestJson) => Call(&NativeMethods.ds_diff_run, requestJson);

    /// <summary>Like <see cref="DiffRun"/> for two texts: <c>{"args":["-u"],"left":"..","right":"..","leftName":"a","rightName":"b"}</c>.</summary>
    public static string DiffTexts(string requestJson) => Call(&NativeMethods.ds_diff_texts, requestJson);

    /// <summary>Applies a patch (backlog E11.5-S16): a unified or git mailbox patch, SEARCH/REPLACE blocks, JSON
    /// Patch (RFC 6902) or a byte script, to files under <c>root</c> or to a <c>text</c>. All or nothing unless
    /// <c>partial</c>; <c>dryRun</c> writes nothing. A patch that does not apply is a result with <c>ok: false</c>.</summary>
    public static string PatchApply(string requestJson) => Call(&NativeMethods.ds_patch_apply, requestJson);

    /// <summary>JSON Patch on documents: <c>{"op":"diff","left":..,"right":..}</c> or
    /// <c>{"op":"apply","doc":..,"patch":[..]}</c>.</summary>
    public static string JsonPatch(string requestJson) => Call(&NativeMethods.ds_json_patch, requestJson);

    /// <summary>Which bytes differ between two files, as runs and as <c>fc /b</c>, <c>cmp -l</c> or script text.</summary>
    public static string BytesDiff(string requestJson) => Call(&NativeMethods.ds_bytes_diff, requestJson);

    /// <summary>Parses how another program started the tool (a JSON array of the arguments):
    /// compare, merge, git external diff, select left, compare selected.</summary>
    public static string ParseArgs(string argsJson) => Call(&NativeMethods.ds_tool_parse_args, argsJson);

    /// <summary>The built-in ignore list: <c>*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved</c>.</summary>
    public static string DefaultIgnore()
    {
        DsBuf b = default;
        Native.Check(NativeMethods.ds_tool_default_ignore(&b));
        return Native.TakeBuf(b);
    }

    /// <summary>Whether <paramref name="path"/> matches the pattern list.</summary>
    public static bool IsIgnored(string patterns, string path)
    {
        var pat = Encoding.UTF8.GetBytes(patterns);
        byte result = 0;
        fixed (byte* pp = pat)
        fixed (char* pc = path)
        {
            Native.Check(NativeMethods.ds_tool_is_ignored(
                new DsStr8 { ptr = pp, len = (nuint)pat.Length },
                new DsStr16 { ptr = (ushort*)pc, len = (nuint)path.Length },
                &result));
        }
        return result != 0;
    }

    /// <summary>Three-way merge of three texts (a stub: only the trivial cases are decided).
    /// Request <c>{"base":"","local":"","remote":""}</c>; report <c>{"outcome":"clean"|"conflict",...}</c>.</summary>
    public static string Merge3(string requestJson) => Call(&NativeMethods.ds_merge3, requestJson);

    /// <summary>The merge-tool contract on files: <c>{"local","remote","base","output"}</c> in, <c>{"exitCode","message"}</c> out.</summary>
    public static string MergeFiles(string requestJson) => Call(&NativeMethods.ds_merge_files, requestJson);

    /// <summary>Remembers <paramref name="path"/> as the left side of the next compare (Explorer "Select left side").</summary>
    public static void SetLeftSelection(string path)
    {
        fixed (char* p = path)
        {
            Native.Check(NativeMethods.ds_shell_selection_set(new DsStr16 { ptr = (ushort*)p, len = (nuint)path.Length }));
        }
    }

    /// <summary>The remembered left side as JSON (<c>{"left":..,"chosenAt":..}</c>), or null when there is none.</summary>
    public static string? GetLeftSelection()
    {
        DsBuf b = default;
        Native.Check(NativeMethods.ds_shell_selection_get(&b));
        var s = Native.TakeBuf(b);
        return s.Length == 0 ? null : s;
    }

    public static void ClearLeftSelection() => Native.Check(NativeMethods.ds_shell_selection_clear());

    /// <summary>Free and total bytes of the volume that holds <paramref name="path"/> (Windows).</summary>
    public static (ulong Free, ulong Total) VolumeInfo(string path)
    {
        DsVolumeInfo info = default;
        fixed (char* p = path)
        {
            Native.Check(NativeMethods.ds_volume_info(new DsStr16 { ptr = (ushort*)p, len = (nuint)path.Length }, &info));
        }
        return (info.free_bytes, info.total_bytes);
    }
}
