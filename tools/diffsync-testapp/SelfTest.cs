// Engine scenarios without a window (backlog E10-S2): the numbers behind "1M rows scroll smoothly".
// A frame is what the text control does per draw: the rows, the line text of both sides and the
// intraline highlights of the changed rows. Exit code 0 only when every budget is met.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using DiffSync.Native;

namespace DiffSync.TestApp;

internal static class SelfTest
{
    private const double FrameBudgetMs = 16.0;
    private static bool _ok = true;

    private static void Check(bool cond, string what)
    {
        Console.WriteLine((cond ? "  ok   " : "  FAIL ") + what);
        if (!cond) _ok = false;
    }

    public static int Run(string[] args)
    {
        var lines = 1_000_000;
        var i = Array.IndexOf(args, "--lines");
        if (i >= 0 && i + 1 < args.Length) lines = int.Parse(args[i + 1]);
        Console.WriteLine($"diffsync self test, ABI {DiffSyncLibrary.AbiMajor}.{DiffSyncLibrary.AbiMinor}, {lines:N0} lines");
        var dir = Directory.CreateTempSubdirectory("dstestapp").FullName;
        try
        {
            TextScroll(dir, lines);
            FolderGrid(dir);
            SyncQuestion(dir);
        }
        finally
        {
            try { Directory.Delete(dir, true); } catch { }
        }
        Console.WriteLine(_ok ? "SELF TEST PASSED" : "SELF TEST FAILED");
        return _ok ? 0 : 1;
    }

    private static string LogFile(string path, int lines, bool variant)
    {
        using var w = new StreamWriter(path, false, new UTF8Encoding(false), 1 << 20);
        for (var n = 0; n < lines; n++)
        {
            if (variant && n % 20_000 == 7) continue; // a dropped line
            var tag = variant && n % 5000 == 3 ? "CHANGED" : "INFO ";
            w.Write("2026-11-22 10:");
            w.Write((n % 60).ToString("00"));
            w.Write(':');
            w.Write(((n / 60) % 60).ToString("00"));
            w.Write(' ');
            w.Write(tag);
            w.Write(" worker-");
            w.Write(n % 8);
            w.Write(" request ");
            w.Write(n);
            w.Write(" handled in ");
            w.Write(n % 250);
            w.Write(" ms\n");
        }
        return path;
    }

    private static void TextScroll(string dir, int lines)
    {
        Console.WriteLine("1. text compare and scroll");
        var sw = Stopwatch.StartNew();
        var l = LogFile(Path.Combine(dir, "l.log"), lines, false);
        var r = LogFile(Path.Combine(dir, "r.log"), lines, true);
        Console.WriteLine($"  generated two files in {sw.Elapsed.TotalSeconds:F1} s");
        sw.Restart();
        using var tc = TextCompare.Open(l, r);
        SpinWait.SpinUntil(() => tc.Status.State is TextState.Done or TextState.Failed, 180_000);
        var st = tc.Status;
        Check(st.State == TextState.Done, $"aligned {st.Rows:N0} rows in {sw.Elapsed.TotalSeconds:F1} s");
        Check(st.Rows >= (uint)lines, $"{st.Rows:N0} rows >= {lines:N0}");

        var rows = new TextRow[128];
        var buf = new char[1024];
        var frames = new List<double>();
        var rng = new Random(1);
        var big = st.Rows > 100 ? st.Rows - 100 : 1;
        var sections = Math.Max(1, (int)st.Sections);
        // 4000 frames: smooth scrolling, big jumps, and jumps to differences (F3)
        long top = 0;
        for (var f = 0; f < 4000; f++)
        {
            top = f % 4 switch
            {
                0 => Math.Min(top + 3, big),
                1 => (long)(rng.NextDouble() * big),
                2 => Math.Min(top + 1, big),
                _ => tc.FindDiff((uint)Math.Min(top, big), true, false) ?? 0,
            };
            var t = Stopwatch.GetTimestamp();
            var n = tc.GetRows((uint)top, rows.AsSpan(0, 60));
            for (var k = 0; k < n; k++)
            {
                if (rows[k].LeftLine != TextRow.Absent) tc.TryGetLine(0, rows[k].LeftLine, buf, out _, out _);
                if (rows[k].RightLine != TextRow.Absent) tc.TryGetLine(1, rows[k].RightLine, buf, out _, out _);
                if (rows[k].Kind == RowKind.Different) tc.GetIntraline((uint)top + (uint)k);
            }
            frames.Add(Stopwatch.GetElapsedTime(t).TotalMilliseconds);
        }
        frames.Sort();
        double P(double q) => frames[(int)Math.Min(frames.Count - 1, q * frames.Count)];
        Console.WriteLine($"  frames: avg {frames.Average():F3} ms, p50 {P(.5):F3}, p99 {P(.99):F3}, p99.9 {P(.999):F3}, max {frames[^1]:F3} ms ({frames.Count} frames)");
        Check(P(.99) <= FrameBudgetMs, $"p99 {P(.99):F3} ms <= {FrameBudgetMs} ms (engine calls per frame)");
        Check(frames[^1] <= FrameBudgetMs, $"no frame above {FrameBudgetMs} ms (max {frames[^1]:F3} ms)");
        var pull = new Stopwatch();
        pull.Start();
        for (var k = 0; k < 2000; k++) tc.GetRows((uint)rng.Next(0, (int)big), rows.AsSpan(0, 100));
        pull.Stop();
        Check(pull.Elapsed.TotalMilliseconds / 2000 <= 0.2, $"100-row pull {pull.Elapsed.TotalMilliseconds / 2000:F4} ms <= 0.2 ms");
        _ = sections;
    }

    private static void FolderGrid(string dir)
    {
        Console.WriteLine("2. folder grid");
        var l = Directory.CreateDirectory(Path.Combine(dir, "FL")).FullName;
        var r = Directory.CreateDirectory(Path.Combine(dir, "FR")).FullName;
        for (var d = 0; d < 100; d++)
        {
            Directory.CreateDirectory(Path.Combine(l, "d" + d));
            Directory.CreateDirectory(Path.Combine(r, "d" + d));
            for (var f = 0; f < 200; f++)
            {
                File.WriteAllText(Path.Combine(l, "d" + d, "f" + f + ".txt"), "x");
                if (f % 20 != 7) File.WriteAllText(Path.Combine(r, "d" + d, "f" + f + ".txt"), f % 10 == 3 ? "yy" : "x");
            }
        }
        var sw = Stopwatch.StartNew();
        using var fc = FolderCompare.Open(l, r);
        SpinWait.SpinUntil(() => fc.Status.State != FolderState.Scanning, 120_000);
        var st = fc.Status;
        Check(st.State == FolderState.Done, $"scanned {st.NodeCount:N0} nodes in {sw.Elapsed.TotalSeconds:F2} s");
        using var view = fc.CreateView();
        view.Set("{\"filter\":\"differences\"}");
        view.Expand(0, true, recursive: false);
        var (count, _) = view.RowCount();
        var rows = new DirRow[100];
        var frames = new List<double>();
        for (var k = 0; k < 2000; k++)
        {
            var t = Stopwatch.GetTimestamp();
            var n = view.GetRows((uint)(k % Math.Max(1, (int)count - 50)), rows);
            for (var j = 0; j < n; j++) fc.GetName(rows[j].Node);
            frames.Add(Stopwatch.GetElapsedTime(t).TotalMilliseconds);
        }
        frames.Sort();
        Console.WriteLine($"  {count:N0} visible rows; frame max {frames[^1]:F3} ms, p99 {frames[(int)(frames.Count * .99)]:F3} ms");
        Check(frames[^1] <= FrameBudgetMs, $"folder grid frames within {FrameBudgetMs} ms");
    }

    private static void SyncQuestion(string dir)
    {
        Console.WriteLine("3. sync dry run and the question handshake");
        var l = Directory.CreateDirectory(Path.Combine(dir, "SL")).FullName;
        var r = Directory.CreateDirectory(Path.Combine(dir, "SR")).FullName;
        File.WriteAllText(Path.Combine(l, "f.txt"), "left");
        File.WriteAllText(Path.Combine(r, "f.txt"), "old right");
        File.WriteAllText(Path.Combine(l, "new.txt"), "new");
        using var fc = FolderCompare.Open(l, r);
        SpinWait.SpinUntil(() => fc.Status.State != FolderState.Scanning, 60_000);
        using var plan = fc.Plan("{\"preset\":\"mirrorToRight\"}");
        var s = plan.Summary;
        Check(s.CopiesToRight == 2, $"dry run: {s.CopiesToRight} copies, nothing written yet");
        Check(!File.Exists(Path.Combine(r, "new.txt")), "a plan writes nothing");
        File.WriteAllText(Path.Combine(r, "f.txt"), "edited behind our back"); // plan.md §9.1
        var asked = new TaskCompletionSource<ulong>();
        var callbackThread = 0;
        using var job = plan.Execute(null, e =>
        {
            if (e.Kind == EventKind.Question) { callbackThread = Environment.CurrentManagedThreadId; asked.TrySetResult(e.A); }
        });
        var id = asked.Task.Wait(30_000) ? asked.Task.Result : 0;
        Check(id != 0 && callbackThread != Environment.CurrentManagedThreadId, "the question arrived on a core thread");
        var q = job.GetQuestion();
        Check(q is { Kind: QuestionKind.ChangedSinceCompare }, "the question says the file changed since the compare");
        Task.Run(() => job.Answer(id, Answer.Skip)).Wait();
        SpinWait.SpinUntil(() => job.Status.State == JobState.Finished, 30_000);
        var js = job.Status;
        Check(js is { Skipped: 1, Done: 1 }, $"answered from another thread: done {js.Done}, skipped {js.Skipped}");
        Check(File.ReadAllText(Path.Combine(r, "f.txt")) == "edited behind our back", "the edited file was not overwritten");
        Check(File.Exists(Path.Combine(r, "new.txt")), "the other action ran");
    }
}
