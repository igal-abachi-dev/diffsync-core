// xUnit tests of the ABI 1.1 integration helpers (backlog E11.5-S5.2).
using System;
using System.Linq;
using System.Text.Json;
using System.Threading;
using DiffSync.Native;
using Xunit;

namespace DiffSync.Native.Tests;

public class ToolsTests
{
    [Fact]
    public void Diff_parse_ignore_selection_and_merge()
    {
        using TempDir d = new();
        var a = d.File("a.txt", "1\n2\n3\n");
        var b = d.File("b.txt", "1\nTWO\n3\n");
        using var run = JsonDocument.Parse(Tools.DiffRun(JsonSerializer.Serialize(new { args = new[] { "-u", a, b } })));
        Assert.Equal(1, run.RootElement.GetProperty("exitCode").GetInt32());
        Assert.Contains("+TWO", run.RootElement.GetProperty("stdout").GetString());

        using var texts = JsonDocument.Parse(Tools.DiffTexts(JsonSerializer.Serialize(
            new { args = new[] { "-u" }, left = "x\n", right = "y\n", leftName = "old", rightName = "new" })));
        Assert.Contains("--- old", texts.RootElement.GetProperty("stdout").GetString());

        using var parsed = JsonDocument.Parse(Tools.ParseArgs(JsonSerializer.Serialize(new[] { "L", "R", "B", "-o", "M" })));
        Assert.Equal("merge", parsed.RootElement.GetProperty("invocation").GetProperty("kind").GetString());
        var ex = Assert.Throws<DiffSyncException>(() => Tools.ParseArgs("[\"only-one\"]"));
        Assert.Equal(DsError.InvalidArg, ex.Kind);

        var preset = Tools.DefaultIgnore();
        Assert.True(Tools.IsIgnored(preset, @"D:\repo\web\package-lock.json"));
        Assert.False(Tools.IsIgnored(preset, @"D:\repo\web\package.json"));

        using var merged = JsonDocument.Parse(Tools.Merge3(JsonSerializer.Serialize(new { @base = "a\n", local = "a\n", remote = "b\n" })));
        Assert.Equal("clean", merged.RootElement.GetProperty("outcome").GetString());

        var state = d.Dir("state");
        Environment.SetEnvironmentVariable("DIFFSYNC_STATE_DIR", state);
        try
        {
            Assert.Null(Tools.GetLeftSelection());
            Tools.SetLeftSelection(a);
            Assert.Contains("a.txt", Tools.GetLeftSelection());
            Tools.ClearLeftSelection();
            Assert.Null(Tools.GetLeftSelection());
        }
        finally
        {
            Environment.SetEnvironmentVariable("DIFFSYNC_STATE_DIR", null);
        }

        var (free, total) = Tools.VolumeInfo(d.Path);
        Assert.True(total > 0 && free <= total);
    }

    [Fact]
    public void Text_folds_section_kinds_and_find()
    {
        using TempDir d = new();
        var lines = string.Concat(Enumerable.Range(1, 30).Select(i => $"line {i}\n"));
        var left = d.File("l.txt", lines);
        var right = d.File("r.txt", lines.Replace("line 15\n", "CHANGED\n").Replace("line 25\n", ""));
        using var t = TextCompare.Open(left, right);
        SpinWait.SpinUntil(() => t.Status.State == TextState.Done, 20_000);

        var folds = t.GetFolds(3);
        Assert.Equal(3, folds.Length);
        Assert.Equal(new Fold(0, 11), folds[0]);

        var kinds = new SectionKind[4];
        var n = t.GetSectionKinds(0, kinds);
        Assert.Equal(new[] { SectionKind.Replace, SectionKind.Delete }, kinds[..n]);
        Assert.NotNull(t.FindSection(0, true, SectionKind.Delete));
        Assert.Null(t.FindSection(0, true, SectionKind.Insert));

        var hit = t.Find("changed", FindFlags.None, 0);
        Assert.Equal(new FindHit(14, true, 14, 0, 7), hit);
        Assert.Null(t.Find("changed", FindFlags.CaseSensitive, 0));
    }
}

public class EditTests
{
    [Fact]
    public void Copy_a_section_edit_a_range_save_and_apply_a_patch()
    {
        using TempDir d = new();
        var l = d.File("l.txt", "a\nb\nc\n");
        var r = d.File("r.txt", "a\nB\nc\n");
        using var t = TextCompare.Open(l, r);
        SpinWait.SpinUntil(() => t.Status.State == TextState.Done, 20_000);
        using var copied = JsonDocument.Parse(t.Edit("{\"op\":\"copySection\",\"from\":\"left\",\"section\":0}"));
        Assert.True(copied.RootElement.GetProperty("changed").GetBoolean());
        Assert.True(copied.RootElement.GetProperty("right").GetProperty("dirty").GetBoolean());
        // a host buffer's range edit, with the restart of the compare deferred
        using var typed = JsonDocument.Parse(t.Edit(
            "{\"op\":\"replaceRange\",\"side\":\"right\",\"startLine\":2,\"startColumn\":1,\"endLine\":2,\"endColumn\":1,\"text\":\"!\",\"refresh\":false}"));
        Assert.True(typed.RootElement.GetProperty("stale").GetBoolean());
        t.Edit("{\"op\":\"refresh\"}");
        using var saved = JsonDocument.Parse(t.Save("{\"side\":\"right\",\"backupSuffix\":\".bak\"}"));
        Assert.Equal(7, saved.RootElement.GetProperty("bytes").GetInt32());
        Assert.Equal("a\nb\nc!\n", System.IO.File.ReadAllText(r));
        Assert.Equal("a\nB\nc\n", System.IO.File.ReadAllText(r + ".bak"));
        var ex = Assert.Throws<DiffSyncException>(() => t.Edit("{\"op\":\"nonsense\"}"));
        Assert.Equal(DsError.InvalidArg, ex.Kind);

        var patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n x\n-y\n+Y\n";
        d.File("f.txt", "x\ny\n");
        using var applied = JsonDocument.Parse(Tools.PatchApply(JsonSerializer.Serialize(new { patch, root = d.Path })));
        Assert.True(applied.RootElement.GetProperty("written").GetBoolean());
        Assert.Equal("x\nY\n", System.IO.File.ReadAllText(System.IO.Path.Combine(d.Path, "f.txt")));
        using var json = JsonDocument.Parse(Tools.JsonPatch("{\"op\":\"diff\",\"left\":{\"x\":1},\"right\":{\"x\":2}}"));
        Assert.Equal("replace", json.RootElement.GetProperty("patch")[0].GetProperty("op").GetString());
    }

    [Fact]
    public void Ignore_marks_and_rename_through_the_folder_wrapper()
    {
        using TempDir d = new();
        var l = d.Dir("l");
        var r = d.Dir("r");
        System.IO.File.WriteAllText(System.IO.Path.Combine(l, "only.txt"), "1");
        using var f = FolderCompare.Open(l, r);
        SpinWait.SpinUntil(() => f.Status.State != FolderState.Scanning, 20_000);
        using var v = f.CreateView("{\"filter\":\"leftOrphans\"}");
        Assert.Equal(1u, v.RowCount().Count);
        f.SetIgnored(new uint[] { 1 }, true);
        Assert.Equal(0u, v.RowCount().Count);
        v.Set("{\"showIgnored\":true}");
        Assert.Equal(1u, v.RowCount().Count);
        using var job = FileOps.Rename(System.IO.Path.Combine(l, "only.txt"), "renamed.txt");
        SpinWait.SpinUntil(() => System.IO.File.Exists(System.IO.Path.Combine(l, "renamed.txt")), 20_000);
        Assert.False(System.IO.File.Exists(System.IO.Path.Combine(l, "only.txt")));
    }
}
