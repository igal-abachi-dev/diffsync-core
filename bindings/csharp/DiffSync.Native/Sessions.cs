// High-level wrappers over the C ABI. Pulls fill caller-supplied spans (pool them with
// ArrayPool<T>): no per-row allocation. Strings go in as `fixed char*` + length.
using System;
using System.Text;

namespace DiffSync.Native;

/// <summary>Common base: owns the safe handle and the event pump.</summary>
public abstract unsafe class CoreObject : IDisposable
{
    internal DsSafeHandle Handle { get; private protected set; } = new();
    private protected EventPump? Pump;

    /// <summary>The raw ABI handle (for diagnostics).</summary>
    public ulong RawHandle => Handle.Value;

    internal ulong H => Handle.IsInvalid || Handle.IsClosed ? throw new ObjectDisposedException(GetType().Name) : Handle.Value;

    public void Dispose()
    {
        Handle.Dispose();
        Pump?.Dispose();
        GC.SuppressFinalize(this);
    }
}

/// <summary>A text compare of two files.</summary>
public sealed unsafe class TextCompare : CoreObject
{
    private TextCompare() { }

    /// <summary>Starts a compare. Events (state, rows) arrive on core threads; give
    /// <paramref name="post"/> (for example <c>Dispatcher.UIThread.Post</c>) to get them on yours.</summary>
    public static TextCompare Open(string left, string right, string? optionsJson = null,
        Action<DsEvent>? onEvent = null, Action<Action>? post = null)
    {
        var t = new TextCompare { Pump = EventPump.Create(onEvent, post) };
        var json = Native.Json(optionsJson);
        ulong h = 0;
        fixed (char* l = left)
        fixed (char* r = right)
        fixed (byte* j = json)
        {
            var status = NativeMethods.ds_text_open(
                new DsStr16 { ptr = (ushort*)l, len = (nuint)left.Length },
                new DsStr16 { ptr = (ushort*)r, len = (nuint)right.Length },
                new DsStr8 { ptr = j, len = (nuint)json.Length },
                t.Pump is null ? null : EventPump.Callback, t.Pump is null ? null : t.Pump.User, &h);
            if (status < 0) { t.Pump?.Dispose(); Native.Check(status); }
        }
        t.Handle = new DsSafeHandle(h);
        return t;
    }

    public TextStatus Status
    {
        get
        {
            DsTextStatus s = default;
            Native.Check(NativeMethods.ds_text_status(H, &s));
            return new TextStatus((TextState)s.state, s.rows, s.generation, s.same, s.important_diff,
                s.unimportant_diff, s.left_orphan, s.right_orphan, s.unimportant_left_orphan,
                s.unimportant_right_orphan, s.sections, s.important_sections);
        }
    }

    /// <summary>The failure message when the state is <see cref="TextState.Failed"/>.</summary>
    public string? FailureMessage => Status.State == TextState.Failed ? Native.LastError() : null;

    /// <summary>Fills <paramref name="rows"/> with the rows from <paramref name="first"/>; returns how many.</summary>
    public int GetRows(uint first, Span<TextRow> rows)
    {
        uint written = 0;
        fixed (TextRow* p = rows)
        {
            Native.Check(NativeMethods.ds_text_get_rows(H, first, (uint)rows.Length, (DsTextRow*)p, &written));
        }
        return (int)written;
    }

    public int GetSections(uint first, Span<Section> sections)
    {
        uint written = 0;
        fixed (Section* p = sections)
        {
            Native.Check(NativeMethods.ds_text_get_sections(H, first, (uint)sections.Length, (DsSection*)p, &written));
        }
        return (int)written;
    }

    /// <summary>Copies a line into <paramref name="buffer"/>; <paramref name="length"/> is the full
    /// length even when the buffer is too small (then the result is false).</summary>
    public bool TryGetLine(int side, uint line, Span<char> buffer, out int length, out LineEnding ending)
    {
        uint len = 0;
        DsLineInfo info = default;
        int status;
        fixed (char* p = buffer)
        {
            status = NativeMethods.ds_text_get_line(H, (byte)side, line, (ushort*)p, (uint)buffer.Length, &len, &info);
        }
        length = (int)len;
        ending = (LineEnding)info.ending;
        if (status == (int)DsError.BufferTooSmall) return false;
        Native.Check(status);
        return true;
    }

    public string GetLine(int side, uint line)
    {
        var buf = new char[256];
        while (true)
        {
            if (TryGetLine(side, line, buf, out var len, out _)) return new string(buf, 0, len);
            buf = new char[len];
        }
    }

    /// <summary>The word-level highlights of a paired row.</summary>
    public (TextRange[] Left, TextRange[] Right) GetIntraline(uint row)
    {
        uint ln = 0, rn = 0;
        var status = NativeMethods.ds_text_get_intraline(H, row, null, 0, &ln, null, 0, &rn);
        if (status != (int)DsError.BufferTooSmall) Native.Check(status);
        var left = new TextRange[ln];
        var right = new TextRange[rn];
        fixed (TextRange* l = left)
        fixed (TextRange* r = right)
        {
            Native.Check(NativeMethods.ds_text_get_intraline(H, row, (DsRange*)l, ln, &ln, (DsRange*)r, rn, &rn));
        }
        return (left, right);
    }

    /// <summary>The next (or previous) difference row, or null at the end.</summary>
    public uint? FindDiff(uint fromRow, bool forward, bool importantOnly)
    {
        uint row = 0;
        var status = NativeMethods.ds_text_find_diff(H, fromRow, forward ? 1 : -1, (byte)(importantOnly ? 1 : 0), &row);
        if (status == (int)DsError.NotFound) return null;
        Native.Check(status);
        return row;
    }

    public void GetOverview(Span<byte> buckets)
    {
        fixed (byte* p = buckets)
        {
            Native.Check(NativeMethods.ds_text_get_overview(H, p, (uint)buckets.Length));
        }
    }

    public DocInfo GetDocInfo(int side)
    {
        DsDocInfo d = default;
        Native.Check(NativeMethods.ds_text_doc_info(H, (byte)side, &d));
        var counts = new ulong[4];
        for (var i = 0; i < 4; i++) counts[i] = d.ending_counts[i];
        var n = 0;
        while (n < 24 && d.encoding[n] != 0) n++;
        return new DocInfo(d.line_count, d.is_binary != 0, d.has_bom != 0, d.byte_len, counts,
            Encoding.UTF8.GetString(d.encoding, n));
    }

    /// <summary>Aligns again with other options; <see cref="TextStatus.Generation"/> increases.</summary>
    public void Recompare(string optionsJson)
    {
        var json = Native.Json(optionsJson);
        fixed (byte* j = json)
        {
            Native.Check(NativeMethods.ds_text_recompare(H, new DsStr8 { ptr = j, len = (nuint)json.Length }));
        }
    }

    public void Cancel() => Native.Check(NativeMethods.ds_text_cancel(H));
}

/// <summary>A folder compare.</summary>
public sealed unsafe class FolderCompare : CoreObject
{
    private FolderCompare() { }

    public static FolderCompare Open(string leftRoot, string rightRoot, string? optionsJson = null,
        Action<DsEvent>? onEvent = null, Action<Action>? post = null)
    {
        var f = new FolderCompare { Pump = EventPump.Create(onEvent, post) };
        var json = Native.Json(optionsJson);
        ulong h = 0;
        fixed (char* l = leftRoot)
        fixed (char* r = rightRoot)
        fixed (byte* j = json)
        {
            var status = NativeMethods.ds_folder_open(
                new DsStr16 { ptr = (ushort*)l, len = (nuint)leftRoot.Length },
                new DsStr16 { ptr = (ushort*)r, len = (nuint)rightRoot.Length },
                new DsStr8 { ptr = j, len = (nuint)json.Length },
                f.Pump is null ? null : EventPump.Callback, f.Pump is null ? null : f.Pump.User, &h);
            if (status < 0) { f.Pump?.Dispose(); Native.Check(status); }
        }
        f.Handle = new DsSafeHandle(h);
        return f;
    }

    public FolderStatus Status
    {
        get
        {
            DsFolderStatus s = default;
            Native.Check(NativeMethods.ds_folder_status(H, &s));
            return new FolderStatus((FolderState)s.state, s.stage, s.progress_done, s.progress_total, s.node_count,
                s.warning_count, s.sync_blocked != 0, s.diff, s.left_newer, s.right_newer, s.left_orphan,
                s.right_orphan, s.error, s.items, s.revision);
        }
    }

    /// <summary>The failure message when the state is <see cref="FolderState.Failed"/>.</summary>
    public string? FailureMessage => Status.State == FolderState.Failed ? Native.LastError() : null;

    public DirRow GetNode(uint node)
    {
        DirRow row = default;
        Native.Check(NativeMethods.ds_folder_node_info(H, node, (DsDirRow*)&row));
        return row;
    }

    /// <summary>The name of a node: side 0 left, 1 right, 2 display.</summary>
    public string GetName(uint node, int side = 2)
    {
        var buf = new char[128];
        while (true)
        {
            uint len = 0;
            int status;
            fixed (char* p = buf)
            {
                status = NativeMethods.ds_folder_node_name(H, node, (byte)side, (ushort*)p, (uint)buf.Length, &len);
            }
            if (status == (int)DsError.BufferTooSmall) { buf = new char[len]; continue; }
            Native.Check(status);
            return new string(buf, 0, (int)len);
        }
    }

    /// <summary>A JSON report: warnings, per-node errors, <c>syncBlocked</c>.</summary>
    public string Report()
    {
        DsBuf b = default;
        Native.Check(NativeMethods.ds_folder_report(H, &b));
        return Native.TakeBuf(b);
    }

    public void CompareContent(ReadOnlySpan<uint> nodes)
    {
        fixed (uint* p = nodes) Native.Check(NativeMethods.ds_folder_compare_content(H, p, (uint)nodes.Length));
    }

    public void Refresh(ReadOnlySpan<uint> nodes)
    {
        fixed (uint* p = nodes) Native.Check(NativeMethods.ds_folder_refresh(H, p, (uint)nodes.Length));
    }

    public void Cancel() => Native.Check(NativeMethods.ds_folder_cancel(H));

    public FolderView CreateView(string? viewJson = null) => FolderView.Create(this, viewJson);

    public SyncPlan Plan(string? rulesJson = null, ReadOnlySpan<uint> selection = default) =>
        SyncPlan.Create(this, rulesJson, selection);
}

/// <summary>A flat view over a folder compare (filter, search, sort, expansion).</summary>
public sealed unsafe class FolderView : CoreObject
{
    private readonly FolderCompare _folder;

    private FolderView(FolderCompare folder) => _folder = folder;

    internal static FolderView Create(FolderCompare folder, string? viewJson)
    {
        var json = Native.Json(viewJson);
        ulong h = 0;
        fixed (byte* j = json)
        {
            Native.Check(NativeMethods.ds_view_create(folder.H, new DsStr8 { ptr = j, len = (nuint)json.Length }, &h));
        }
        return new FolderView(folder) { Handle = new DsSafeHandle(h) };
    }

    /// <summary>Changes filter, search or sort (fields not given stay).</summary>
    public void Set(string viewJson)
    {
        var json = Native.Json(viewJson);
        fixed (byte* j = json)
        {
            Native.Check(NativeMethods.ds_view_set(H, new DsStr8 { ptr = j, len = (nuint)json.Length }));
        }
    }

    public void Expand(uint node, bool expanded, bool recursive = false) =>
        Native.Check(NativeMethods.ds_view_expand(H, node, (byte)(expanded ? 1 : 0), (byte)(recursive ? 1 : 0)));

    /// <summary>The visible row count and the generation (it changes whenever rows may have).</summary>
    public (uint Count, ulong Generation) RowCount()
    {
        uint c = 0;
        ulong g = 0;
        Native.Check(NativeMethods.ds_view_row_count(H, &c, &g));
        return (c, g);
    }

    public int GetRows(uint first, Span<DirRow> rows)
    {
        uint written = 0;
        fixed (DirRow* p = rows)
        {
            Native.Check(NativeMethods.ds_view_get_rows(H, first, (uint)rows.Length, (DsDirRow*)p, &written));
        }
        return (int)written;
    }
}

/// <summary>A sync preview. Nothing is written until <see cref="Execute"/>.</summary>
public sealed unsafe class SyncPlan : CoreObject
{
    private SyncPlan() { }

    internal static SyncPlan Create(FolderCompare folder, string? rulesJson, ReadOnlySpan<uint> selection)
    {
        var json = Native.Json(rulesJson);
        ulong h = 0;
        fixed (byte* j = json)
        fixed (uint* s = selection)
        {
            Native.Check(NativeMethods.ds_sync_plan(folder.H, new DsStr8 { ptr = j, len = (nuint)json.Length },
                selection.IsEmpty ? null : s, (uint)selection.Length, &h));
        }
        return new SyncPlan { Handle = new DsSafeHandle(h) };
    }

    public PlanSummary Summary
    {
        get
        {
            DsPlanSummary s = default;
            Native.Check(NativeMethods.ds_plan_summary(H, &s));
            return new PlanSummary(s.copies_to_left, s.copies_to_right, s.folders_created, s.deletes_left,
                s.deletes_right, s.conflicts, s.errors, s.skipped_links, s.bytes_to_left, s.bytes_to_right,
                s.action_count, s.filters_active != 0);
        }
    }

    public int GetActions(uint first, Span<PlanAction> actions)
    {
        uint written = 0;
        fixed (PlanAction* p = actions)
        {
            Native.Check(NativeMethods.ds_plan_get_actions(H, first, (uint)actions.Length, (DsPlanAction*)p, &written));
        }
        return (int)written;
    }

    /// <summary>The nodes whose choice is still open (Leave is preselected).</summary>
    public uint[] GetPrompts()
    {
        var all = new uint[Summary.Conflicts];
        uint written = 0;
        fixed (uint* p = all)
        {
            Native.Check(NativeMethods.ds_plan_get_prompts(H, 0, (uint)all.Length, p, &written));
        }
        return all.AsSpan(0, (int)written).ToArray();
    }

    /// <summary>JSON: issues, skipped links, prompts and the dry-run lines.</summary>
    public string Report()
    {
        DsBuf b = default;
        Native.Check(NativeMethods.ds_plan_report(H, &b));
        return Native.TakeBuf(b);
    }

    public void SetOp(uint node, SyncOp op) => Native.Check(NativeMethods.ds_plan_set_op(H, node, (byte)op));

    /// <summary>Starts the job. Questions arrive as <see cref="EventKind.Question"/> events; answer them
    /// with <see cref="Job.Answer"/> from any thread.</summary>
    public Job Execute(string? execJson = null, Action<DsEvent>? onEvent = null, Action<Action>? post = null) =>
        Job.StartSync(this, execJson, onEvent, post);
}

/// <summary>A running sync or file-operation job.</summary>
public sealed unsafe class Job : CoreObject
{
    private Job() { }

    internal static Job StartSync(SyncPlan plan, string? execJson, Action<DsEvent>? onEvent, Action<Action>? post)
    {
        var job = new Job { Pump = EventPump.Create(onEvent, post) };
        var json = Native.Json(execJson);
        ulong h = 0;
        fixed (byte* j = json)
        {
            var status = NativeMethods.ds_plan_execute(plan.H, new DsStr8 { ptr = j, len = (nuint)json.Length },
                job.Pump is null ? null : EventPump.Callback, job.Pump is null ? null : job.Pump.User, &h);
            if (status < 0) { job.Pump?.Dispose(); Native.Check(status); }
        }
        job.Handle = new DsSafeHandle(h);
        return job;
    }

    internal static Job Adopt(ulong handle, EventPump? pump) => new() { Handle = new DsSafeHandle(handle), Pump = pump };

    public JobStatus Status
    {
        get
        {
            DsJobStatus s = default;
            Native.Check(NativeMethods.ds_job_status(H, &s));
            return new JobStatus((JobState)s.state, s.steps_total, s.steps_done, s.has_question != 0, s.bytes_total,
                s.bytes_done, s.done, s.skipped, s.failed, s.kept, s.bytes_copied, s.aborted != 0, s.cancelled != 0);
        }
    }

    /// <summary>The open question, or null.</summary>
    public Question? GetQuestion()
    {
        var buf = new char[256];
        while (true)
        {
            DsQuestion q = default;
            uint len = 0;
            int status;
            fixed (char* p = buf)
            {
                status = NativeMethods.ds_job_get_question(H, &q, (ushort*)p, (uint)buf.Length, &len);
            }
            if (status == (int)DsError.BufferTooSmall) { buf = new char[len]; continue; }
            Native.Check(status);
            return q.id == 0 ? null : new Question(q.id, (QuestionKind)q.kind, q.step, new string(buf, 0, (int)len));
        }
    }

    /// <summary>Answers a question from any thread; the id must be the open question's.</summary>
    public void Answer(ulong questionId, Answer answer) =>
        Native.Check(NativeMethods.ds_job_answer(H, questionId, (byte)answer));

    public void Pause() => Native.Check(NativeMethods.ds_job_pause(H));

    public void Resume() => Native.Check(NativeMethods.ds_job_resume(H));

    public void Cancel() => Native.Check(NativeMethods.ds_job_cancel(H));

    /// <summary>The JSON result of a finished job (<see cref="DsError.Busy"/> while running).</summary>
    public string ResultJson()
    {
        DsBuf b = default;
        Native.Check(NativeMethods.ds_job_result(H, &b));
        return Native.TakeBuf(b);
    }
}

/// <summary>Copy, move and delete as jobs, with the same progress, questions and log as sync.</summary>
public static unsafe class FileOps
{
    /// <summary>Plan §9.2: finds (and with <paramref name="delete"/> removes) the <c>.dctmp~</c> temp files a crash
    /// left under <paramref name="root"/>. Synchronous and a whole-tree walk: call it off the UI thread.
    /// Returns the JSON report (<c>found</c>, <c>deleted</c>, <c>errors</c>).</summary>
    public static string MaintenanceSweep(string root, bool delete)
    {
        DsBuf b = default;
        fixed (char* r = root)
        {
            Native.Check(NativeMethods.ds_maintenance_sweep(
                new DsStr16 { ptr = (ushort*)r, len = (nuint)root.Length }, delete ? (byte)1 : (byte)0, &b));
        }
        return Native.TakeBuf(b);
    }

    public static Job Copy(string[] items, string destinationFolder, string? optionsJson = null,
        Action<DsEvent>? onEvent = null, Action<Action>? post = null) =>
        Start(items, destinationFolder, optionsJson, onEvent, post, 0);

    public static Job Move(string[] items, string destinationFolder, string? optionsJson = null,
        Action<DsEvent>? onEvent = null, Action<Action>? post = null) =>
        Start(items, destinationFolder, optionsJson, onEvent, post, 1);

    public static Job Delete(string[] items, string? optionsJson = null,
        Action<DsEvent>? onEvent = null, Action<Action>? post = null) =>
        Start(items, null, optionsJson, onEvent, post, 2);

    private static Job Start(string[] items, string? dst, string? optionsJson, Action<DsEvent>? onEvent,
        Action<Action>? post, int op)
    {
        var pump = EventPump.Create(onEvent, post);
        var json = Native.Json(optionsJson);
        var handles = new System.Runtime.InteropServices.GCHandle[items.Length + 1];
        var views = new DsStr16[items.Length];
        ulong h = 0;
        try
        {
            for (var i = 0; i < items.Length; i++)
            {
                handles[i] = System.Runtime.InteropServices.GCHandle.Alloc(items[i], System.Runtime.InteropServices.GCHandleType.Pinned);
                views[i] = new DsStr16 { ptr = (ushort*)handles[i].AddrOfPinnedObject(), len = (nuint)items[i].Length };
            }
            var d = dst ?? "";
            handles[items.Length] = System.Runtime.InteropServices.GCHandle.Alloc(d, System.Runtime.InteropServices.GCHandleType.Pinned);
            var dstView = new DsStr16 { ptr = (ushort*)handles[items.Length].AddrOfPinnedObject(), len = (nuint)d.Length };
            fixed (DsStr16* v = views)
            fixed (byte* j = json)
            {
                var list = new DsStr16List { items = v, count = (nuint)views.Length };
                var opts = new DsStr8 { ptr = j, len = (nuint)json.Length };
                var cb = pump is null ? null : EventPump.Callback;
                var user = pump is null ? null : pump.User;
                var status = op switch
                {
                    0 => NativeMethods.ds_fileops_copy(list, dstView, opts, cb, user, &h),
                    1 => NativeMethods.ds_fileops_move(list, dstView, opts, cb, user, &h),
                    _ => NativeMethods.ds_fileops_delete(list, opts, cb, user, &h),
                };
                if (status < 0) { pump?.Dispose(); Native.Check(status); }
            }
        }
        finally
        {
            foreach (var g in handles) if (g.IsAllocated) g.Free();
        }
        return Job.Adopt(h, pump);
    }
}
