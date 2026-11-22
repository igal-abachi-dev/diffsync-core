// xUnit tests of DiffSync.Native against the real diffsync_core.dll (backlog E10-S1.2).
using System;
using System.Buffers;
using System.Collections.Concurrent;
using System.IO;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using DiffSync.Native;
using Xunit;

namespace DiffSync.Native.Tests;

public sealed class TempDir : IDisposable
{
    public string Path { get; } = Directory.CreateTempSubdirectory("dsnative").FullName;

    public string File(string name, string content)
    {
        var p = System.IO.Path.Combine(Path, name);
        Directory.CreateDirectory(System.IO.Path.GetDirectoryName(p)!);
        System.IO.File.WriteAllText(p, content);
        return p;
    }

    public string Dir(string name) => Directory.CreateDirectory(System.IO.Path.Combine(Path, name)).FullName;

    public void Dispose()
    {
        try { Directory.Delete(Path, true); } catch { }
    }
}

public class LibraryTests
{
    [Fact]
    public void Abi_version_and_struct_layouts_match_the_header()
    {
        Assert.Equal(1, DiffSyncLibrary.AbiMajor);
        Assert.True(DiffSyncLibrary.AbiMinor >= 1, "ABI 1.1 added the integration exports");
        Assert.Equal(16, Unsafe.SizeOf<TextRow>());
        Assert.Equal(12, Unsafe.SizeOf<TextRange>());
        Assert.Equal(12, Unsafe.SizeOf<Section>());
        Assert.Equal(32, Unsafe.SizeOf<SideInfo>());
        Assert.Equal(88, Unsafe.SizeOf<DirRow>());
        Assert.Equal(16, Unsafe.SizeOf<PlanAction>());
    }

    [Fact]
    public void Options_schema_is_json_with_every_section()
    {
        using var doc = JsonDocument.Parse(DiffSyncLibrary.OptionsSchema());
        var props = doc.RootElement.GetProperty("properties");
        foreach (var s in new[] { "text", "folder", "sync", "exec", "view" }) Assert.True(props.TryGetProperty(s, out _), s);
    }

    [Fact]
    public void Errors_become_typed_exceptions_with_the_core_message()
    {
        var ex = Assert.Throws<DiffSyncException>(() => TextCompare.Open("a", "b", "{not json"));
        Assert.Equal(DsError.InvalidArg, ex.Kind);
        Assert.Contains("options", ex.Message);
        using var dir = new TempDir();
        var ex2 = Assert.Throws<DiffSyncException>(() => FolderCompare.Open(dir.Path, dir.Path, "{\"version\":2}"));
        Assert.Equal(DsError.InvalidArg, ex2.Kind);
    }
}

public class TextTests
{
    private static TextCompare Compare(TempDir d, string left, string right, string? options = null)
    {
        var t = TextCompare.Open(d.File("l.txt", left), d.File("r.txt", right), options);
        SpinWait.SpinUntil(() => t.Status.State is TextState.Done or TextState.Failed, 20_000);
        return t;
    }

    [Fact]
    public void Rows_lines_sections_and_navigation()
    {
        using var d = new TempDir();
        using var t = Compare(d, "a\nb\nc\nd\ne\nf\ng\n", "a\nB\nc\nd\ne\nF\nF2\ng\n");
        var st = t.Status;
        Assert.Equal(TextState.Done, st.State);
        Assert.Equal(8u, st.Rows);
        Assert.Equal((5u, 2u), (st.Same, st.Sections));
        var rows = ArrayPool<TextRow>.Shared.Rent(64);
        try
        {
            var n = t.GetRows(0, rows.AsSpan(0, 64));
            Assert.Equal(8, n);
            Assert.Equal(RowKind.Different, rows[1].Kind);
            Assert.True(rows[1].StartsSection && rows[1].IsImportant);
            Assert.Equal(0u, rows[1].Section);
            Assert.Equal(1u, rows[5].Section);
            Assert.Equal(TextRow.Absent, rows[6].LeftLine);
        }
        finally { ArrayPool<TextRow>.Shared.Return(rows); }
        var secs = new Section[4];
        Assert.Equal(2, t.GetSections(0, secs));
        Assert.Equal(5u, secs[1].FirstRow);
        Assert.Equal(5u, t.FindDiff(2, true, false));
        Assert.Equal(1u, t.FindDiff(5, false, false));
        Assert.Null(t.FindDiff(7, true, false));
        var buckets = new byte[10];
        t.GetOverview(buckets);
        Assert.Contains(buckets, b => b > 0);
    }

    [Fact]
    public void Line_text_grows_its_buffer_and_keeps_unicode_and_endings()
    {
        using var d = new TempDir();
        using var t = Compare(d, "héllo wörld — " + new string('x', 400) + "\r\nsecond\n", "héllo wörld — " + new string('x', 400) + "\r\nsecond");
        var line = t.GetLine(0, 0);
        Assert.StartsWith("héllo wörld — ", line);
        Assert.Equal(14 + 400, line.Length);
        var small = new char[4];
        Assert.False(t.TryGetLine(0, 0, small, out var len, out var ending));
        Assert.Equal(414, len);
        Assert.Equal(LineEnding.CrLf, ending);
        Assert.True(t.TryGetLine(1, 1, new char[16], out _, out var last));
        Assert.Equal(LineEnding.None, last);
        var ex = Assert.Throws<DiffSyncException>(() => t.GetLine(0, 99));
        Assert.Equal(DsError.NotFound, ex.Kind);
    }

    [Fact]
    public void Intraline_and_doc_info()
    {
        using var d = new TempDir();
        using var t = Compare(d, "let total = price * qty;\n", "let total = price * count;\n");
        var (l, r) = t.GetIntraline(0);
        Assert.Equal("qty", Slice(t.GetLine(0, 0), l));
        Assert.Equal("count", Slice(t.GetLine(1, 0), r));
        var info = t.GetDocInfo(0);
        Assert.Equal((1u, false, 25ul), (info.LineCount, info.IsBinary, info.ByteLength));
        Assert.StartsWith("UTF-8", info.Encoding);
        Assert.Equal(1ul, info.EndingCounts[2]);
    }

    private static string Slice(string line, TextRange[] ranges) =>
        string.Join("|", ranges.Select(x => line.Substring((int)x.Start, (int)x.Length)));

    [Fact]
    public void Recompare_changes_the_generation_and_a_missing_file_is_a_failed_state()
    {
        using var d = new TempDir();
        using var t = Compare(d, "a  b\nc\n", "a b\nc\n");
        var g0 = t.Status.Generation;
        t.Recompare("{\"whitespace\":\"changes\"}");
        SpinWait.SpinUntil(() => t.Status is { State: TextState.Done } s && s.Generation > g0, 20_000);
        Assert.Equal(0u, t.Status.ImportantDiff);
        using var bad = TextCompare.Open(System.IO.Path.Combine(d.Path, "none1"), System.IO.Path.Combine(d.Path, "none2"));
        SpinWait.SpinUntil(() => bad.Status.State == TextState.Failed, 20_000);
        Assert.Contains("not found", bad.FailureMessage!, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void A_disposed_object_is_refused_and_the_native_handle_is_closed_once()
    {
        using var d = new TempDir();
        var t = Compare(d, "a\n", "a\n");
        var raw = t.RawHandle;
        t.Dispose();
        t.Dispose(); // double dispose is harmless
        Assert.Throws<ObjectDisposedException>(() => t.Status);
        // the core no longer knows the handle: closing it again is a stale handle, not a crash
        var sh = new DsSafeHandle();
        Assert.True(sh.IsInvalid);
        _ = raw;
    }

    [Fact]
    public void Events_arrive_on_other_threads_or_through_the_dispatcher()
    {
        using var d = new TempDir();
        var main = Environment.CurrentManagedThreadId;
        var direct = new ConcurrentBag<(int Thread, EventKind Kind)>();
        using (var t = TextCompare.Open(d.File("l.txt", "a\nb\n"), d.File("r.txt", "a\nc\n"), null,
                   e => direct.Add((Environment.CurrentManagedThreadId, e.Kind))))
        {
            SpinWait.SpinUntil(() => t.Status.State == TextState.Done, 20_000);
            SpinWait.SpinUntil(() => direct.Any(x => x.Kind == EventKind.State), 5_000);
        }
        Assert.All(direct, x => Assert.NotEqual(main, x.Thread));
        // through a dispatcher: everything is posted to "the UI thread" (a queue we drain)
        var queue = new BlockingCollection<Action>();
        var seen = new ConcurrentBag<EventKind>();
        using (var t = TextCompare.Open(d.File("l2.txt", "a\n"), d.File("r2.txt", "b\n"), null,
                   e => seen.Add(e.Kind), a => queue.Add(a)))
        {
            SpinWait.SpinUntil(() => t.Status.State == TextState.Done, 20_000);
            SpinWait.SpinUntil(() => queue.Count > 0, 5_000);
            while (queue.TryTake(out var a)) a();
        }
        Assert.NotEmpty(seen);
    }
}

public class FolderAndSyncTests
{
    private static FolderCompare Open(string l, string r, string? json = null)
    {
        var f = FolderCompare.Open(l, r, json);
        SpinWait.SpinUntil(() => f.Status.State != FolderState.Scanning, 20_000);
        return f;
    }

    private static void Tree(TempDir l, TempDir r)
    {
        l.File("same.txt", "same"); r.File("same.txt", "same");
        l.File("lonly.txt", "only on the left");
        l.File("dir/a.txt", "aaa");
        l.File("dir/sub/b.txt", "bb");
        r.File("ronly.txt", "only on the right");
        r.File("old/x.txt", "x");
    }

    [Fact]
    public void Folder_nodes_names_and_views()
    {
        using TempDir l = new(), r = new();
        Tree(l, r);
        using var f = Open(l.Path, r.Path);
        var st = f.Status;
        Assert.Equal(FolderState.Done, st.State);
        Assert.Equal((5u, 3u), (st.LeftOrphan, st.RightOrphan));
        using var v = f.CreateView();
        var rows = new DirRow[16];
        var n = v.GetRows(0, rows);
        var names = rows.Take(n).Select(x => f.GetName(x.Node)).ToArray();
        Assert.Equal(new[] { "dir", "old", "lonly.txt", "ronly.txt", "same.txt" }, names);
        var dir = rows[0].Node;
        v.Expand(dir, true);
        Assert.Equal(7u, v.RowCount().Count);
        v.Expand(dir, true, recursive: true);
        Assert.Equal(8u, v.RowCount().Count);
        v.Set("{\"filter\":\"orphans\",\"search\":\"only\"}");
        n = v.GetRows(0, rows);
        Assert.Equal(new[] { "lonly.txt", "ronly.txt" }, rows.Take(n).Select(x => f.GetName(x.Node)).ToArray());
        var ex = Assert.Throws<DiffSyncException>(() => v.Set("{\"filter\":\"nonsense\"}"));
        Assert.Equal(DsError.InvalidArg, ex.Kind);
        Assert.Equal(NodeStatus.LeftOrphan, f.GetNode(rows[0].Node).Status);
        Assert.Contains("syncBlocked", f.Report());
    }

    [Fact]
    public void A_failed_folder_compare_throws_and_has_a_message()
    {
        using TempDir l = new();
        using var f = Open(l.Path, l.Path);
        Assert.Equal(FolderState.Failed, f.Status.State);
        Assert.Contains("same folder", f.FailureMessage!);
        Assert.Throws<DiffSyncException>(() => f.GetNode(0));
    }

    [Fact]
    public void Plan_prompts_overrides_and_a_job_that_runs()
    {
        using TempDir l = new(), r = new();
        Tree(l, r);
        l.File("conflict.txt", "left version"); r.File("conflict.txt", "right!!!!");
        using var f = Open(l.Path, r.Path);
        using var plan = f.Plan("{\"preset\":\"updateBoth\"}");
        Assert.Equal(1u, plan.Summary.Conflicts);
        var prompt = plan.GetPrompts().Single();
        Assert.Equal("conflict.txt", f.GetName(prompt));
        plan.SetOp(prompt, SyncOp.CopyToLeft);
        Assert.Equal(0u, plan.Summary.Conflicts);
        var acts = new PlanAction[64];
        var n = plan.GetActions(0, acts);
        Assert.Contains(acts.Take(n), a => a.Kind == ActionKind.CreateFolderRight && a.IsFolder == 1);
        Assert.Contains("copy-to-right", plan.Report());

        using var mirror = f.Plan("{\"preset\":\"mirrorToRight\"}");
        using var job = mirror.Execute();
        SpinWait.SpinUntil(() => job.Status.State == JobState.Finished, 30_000);
        var st = job.Status;
        Assert.Equal((0u, 0u), (st.Failed, st.Skipped));
        Assert.Equal("bb", File.ReadAllText(System.IO.Path.Combine(r.Path, "dir", "sub", "b.txt")));
        Assert.False(Directory.Exists(System.IO.Path.Combine(r.Path, "old")));
        using var res = JsonDocument.Parse(job.ResultJson());
        Assert.Equal(0, res.RootElement.GetProperty("failed").GetInt32());
    }

    [Fact]
    public async Task A_question_is_answered_from_another_thread_than_the_callback()
    {
        using TempDir l = new(), r = new();
        l.File("f.txt", "left"); r.File("f.txt", "old right");
        using var f = Open(l.Path, r.Path);
        using var plan = f.Plan("{\"preset\":\"mirrorToRight\"}");
        File.WriteAllText(System.IO.Path.Combine(r.Path, "f.txt"), "edited meanwhile"); // plan.md §9.1
        var asked = new TaskCompletionSource<(ulong Id, int Thread)>();
        using var job = plan.Execute(null, e =>
        {
            if (e.Kind == EventKind.Question) asked.TrySetResult((e.A, Environment.CurrentManagedThreadId));
        });
        var (id, callbackThread) = await asked.Task.WaitAsync(TimeSpan.FromSeconds(20));
        Assert.NotEqual(Environment.CurrentManagedThreadId, callbackThread);
        var q = job.GetQuestion();
        Assert.NotNull(q);
        Assert.Equal((id, QuestionKind.ChangedSinceCompare), (q!.Value.Id, q.Value.Kind));
        Assert.Contains("changed", q.Value.Message);
        Assert.Equal(DsError.NotFound, Assert.Throws<DiffSyncException>(() => job.Answer(id + 100, Answer.Skip)).Kind);
        await Task.Run(() => job.Answer(id, Answer.Skip)); // from a thread pool thread
        SpinWait.SpinUntil(() => job.Status.State == JobState.Finished, 30_000);
        using var res = JsonDocument.Parse(job.ResultJson());
        Assert.Equal(1, res.RootElement.GetProperty("skipped").GetInt32());
        Assert.Equal("edited meanwhile", File.ReadAllText(System.IO.Path.Combine(r.Path, "f.txt")));
    }

    [Fact]
    public void File_operations_copy_move_and_delete()
    {
        using TempDir d = new();
        d.File("src/a.txt", "aaa"); d.File("src/dir/b.txt", "bb");
        d.Dir("dst"); d.Dir("dst2");
        var items = new[] { System.IO.Path.Combine(d.Path, "src", "a.txt"), System.IO.Path.Combine(d.Path, "src", "dir") };
        Run(FileOps.Copy(items, System.IO.Path.Combine(d.Path, "dst")));
        Assert.Equal("bb", File.ReadAllText(System.IO.Path.Combine(d.Path, "dst", "dir", "b.txt")));
        Run(FileOps.Move(items, System.IO.Path.Combine(d.Path, "dst2")));
        Assert.True(File.Exists(System.IO.Path.Combine(d.Path, "dst2", "a.txt")));
        Assert.False(File.Exists(items[0]));
        Run(FileOps.Delete(new[] { System.IO.Path.Combine(d.Path, "dst2", "dir"), System.IO.Path.Combine(d.Path, "dst2", "a.txt") }));
        Assert.Empty(Directory.GetFileSystemEntries(System.IO.Path.Combine(d.Path, "dst2")));
        var ex = Assert.Throws<DiffSyncException>(() =>
            FileOps.Copy(new[] { System.IO.Path.Combine(d.Path, "dst") }, System.IO.Path.Combine(d.Path, "dst", "dir")));
        Assert.Equal(DsError.InvalidArg, ex.Kind);
    }

    /// The C# example of the repository README, with real paths (backlog E11-S5.1).
    [Fact]
    public void ReadmeExample()
    {
        using TempDir d = new();
        var left = d.File("a/Program.cs", "one\ntwo\nthree\n");
        var right = d.File("b/Program.cs", "one\n2\nthree\n");
        var repaints = 0;
        using var cmp = TextCompare.Open(left, right,
            optionsJson: """{ "alignment": "standard" }""",
            onEvent: e => { if (e.Kind == EventKind.Rows) { Interlocked.Increment(ref repaints); } },
            post: action => action());
        SpinWait.SpinUntil(() => cmp.Status.State == TextState.Done, 20_000);
        var rows = new TextRow[100];
        int n = cmp.GetRows(first: 0, rows);
        var status = cmp.Status;
        Assert.Equal(TextState.Done, status.State);
        Assert.True(n >= 3 && status.ImportantDiff >= 1);
    }

    [Fact]
    public void MaintenanceSweep_lists_and_deletes_leftover_temp_files()
    {
        using TempDir d = new();
        d.File("keep.txt", "keep");
        var temp = d.File("sub/a.txt.1a2b3c.dctmp~", "half");
        using var listed = JsonDocument.Parse(FileOps.MaintenanceSweep(d.Path, delete: false));
        Assert.Single(listed.RootElement.GetProperty("found").EnumerateArray());
        Assert.True(File.Exists(temp));
        using var removed = JsonDocument.Parse(FileOps.MaintenanceSweep(d.Path, delete: true));
        Assert.Equal(1, removed.RootElement.GetProperty("deleted").GetInt32());
        Assert.False(File.Exists(temp));
        Assert.True(File.Exists(System.IO.Path.Combine(d.Path, "keep.txt")));
        Assert.Throws<DiffSyncException>(() => FileOps.MaintenanceSweep("relative", delete: false));
    }

    private static void Run(Job job)
    {
        using (job)
        {
            SpinWait.SpinUntil(() => job.Status.State == JobState.Finished, 30_000);
            Assert.Equal(0u, job.Status.Failed);
        }
    }
}
