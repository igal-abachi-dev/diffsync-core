// Custom-drawn controls: only the visible rows are drawn, so a 1M-row compare never builds one
// control per line (plan.md §10.4). Everything is code, no XAML and no reflection bindings, so
// the app is NativeAOT friendly.
using System;
using System.Buffers;
using System.Diagnostics;
using System.Globalization;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Media;
using DiffSync.Native;

namespace DiffSync.TestApp;

/// <summary>Timing of the engine calls made while drawing: a frame is the rows, the line text and
/// the intraline highlights of one viewport.</summary>
public sealed class FrameStats
{
    public int Frames;
    public double MaxMs;
    public double LastMs;
    public double TotalMs;

    public void Add(double ms)
    {
        Frames++;
        LastMs = ms;
        TotalMs += ms;
        if (ms > MaxMs) MaxMs = ms;
    }

    public override string ToString() => $"engine {LastMs:F3} ms (max {MaxMs:F3}, avg {(Frames == 0 ? 0 : TotalMs / Frames):F3}, {Frames} frames)";
}

/// <summary>A side-by-side text compare, drawn row by row from <c>ds_text_get_rows</c>.</summary>
public sealed class TextCompareControl : Control
{
    private const double LineHeight = 18;
    private const double CharWidth = 8.2;
    private static readonly Typeface Mono = new("Consolas, Cascadia Mono, Courier New");

    private TextCompare? _tc;
    private TextRow[] _rows = new TextRow[128];
    private readonly char[] _line = new char[512];
    private long _top;
    private uint _rowCount;

    public FrameStats Stats { get; } = new();

    public int VisibleRows => Math.Max(1, (int)(Bounds.Height / LineHeight));

    public long TopRow
    {
        get => _top;
        set
        {
            _top = Math.Max(0, Math.Min(value, Math.Max(0, _rowCount - VisibleRows)));
            InvalidateVisual();
            TopChanged?.Invoke(_top);
        }
    }

    public event Action<long>? TopChanged;

    public uint RowCount => _rowCount;

    public void SetCompare(TextCompare? tc)
    {
        _tc = tc;
        _top = 0;
        Refresh();
    }

    /// <summary>Re-reads the row count and redraws.</summary>
    public void Refresh()
    {
        _rowCount = _tc is null ? 0 : _tc.Status.Rows;
        InvalidateVisual();
    }

    protected override void OnPointerWheelChanged(PointerWheelEventArgs e)
    {
        TopRow -= (long)(e.Delta.Y * 3);
        e.Handled = true;
    }

    protected override void OnKeyDown(KeyEventArgs e)
    {
        switch (e.Key)
        {
            case Key.PageDown: TopRow += VisibleRows - 1; break;
            case Key.PageUp: TopRow -= VisibleRows - 1; break;
            case Key.Down: TopRow++; break;
            case Key.Up: TopRow--; break;
            case Key.Home: TopRow = 0; break;
            case Key.End: TopRow = _rowCount; break;
            case Key.F3 or Key.F4: JumpToDifference(e.Key == Key.F3); break;
            default: return;
        }
        e.Handled = true;
    }

    /// <summary>F3 next difference, F4 previous (the original's navigation).</summary>
    public void JumpToDifference(bool forward)
    {
        if (_tc is null) return;
        var from = (uint)Math.Min(_rowCount == 0 ? 0 : _rowCount - 1, forward ? _top + 1 : Math.Max(0, _top - 1));
        if (_tc.FindDiff(from, forward, false) is { } row) TopRow = row;
    }

    private static IBrush Back(RowKind k) => k switch
    {
        RowKind.Same => Brushes.Transparent,
        RowKind.Different => new SolidColorBrush(Color.FromArgb(70, 255, 170, 0)),
        RowKind.LeftOnly or RowKind.UnimportantLeftOnly => new SolidColorBrush(Color.FromArgb(70, 255, 80, 80)),
        RowKind.RightOnly or RowKind.UnimportantRightOnly => new SolidColorBrush(Color.FromArgb(70, 70, 200, 90)),
        _ => new SolidColorBrush(Color.FromArgb(40, 160, 160, 160)),
    };

    public override void Render(DrawingContext ctx)
    {
        base.Render(ctx);
        ctx.FillRectangle(Brushes.Transparent, new Rect(Bounds.Size));
        if (_tc is null) return;
        var visible = VisibleRows + 1;
        if (_rows.Length < visible) _rows = new TextRow[visible];
        var sw = Stopwatch.StartNew();
        var n = _tc.GetRows((uint)_top, _rows.AsSpan(0, visible));
        var half = Bounds.Width / 2;
        for (var i = 0; i < n; i++)
        {
            ref readonly var r = ref _rows[i];
            var y = i * LineHeight;
            ctx.FillRectangle(Back(r.Kind), new Rect(0, y, Bounds.Width, LineHeight));
            DrawSide(ctx, 0, r.LeftLine, r, (uint)(_top + i), 0, y, half);
            DrawSide(ctx, 1, r.RightLine, r, (uint)(_top + i), half, y, half);
        }
        sw.Stop();
        Stats.Add(sw.Elapsed.TotalMilliseconds);
        ctx.DrawLine(new Pen(Brushes.Gray, 1), new Point(half, 0), new Point(half, Bounds.Height));
    }

    private void DrawSide(DrawingContext ctx, int side, uint line, in TextRow row, uint rowIndex, double x, double y, double width)
    {
        if (line == TextRow.Absent || _tc is null) return;
        _tc.TryGetLine(side, line, _line, out var len, out _);
        len = Math.Min(len, _line.Length);
        if (row.Kind == RowKind.Different && _tc.GetIntraline(rowIndex) is var (l, rr))
        {
            foreach (var rg in side == 0 ? l : rr)
            {
                ctx.FillRectangle(new SolidColorBrush(Color.FromArgb(120, 255, 120, 0)),
                    new Rect(x + 4 + rg.Start * CharWidth, y, Math.Max(2, rg.Length * CharWidth), LineHeight));
            }
        }
        var text = new FormattedText(new string(_line, 0, len), CultureInfo.InvariantCulture, FlowDirection.LeftToRight,
            Mono, 13, Brushes.White)
        { MaxTextWidth = width - 6, MaxTextHeight = LineHeight };
        ctx.DrawText(text, new Point(x + 4, y + 1));
    }
}

/// <summary>The folder grid over <c>ds_view_*</c>: one row per visible node, click to expand.</summary>
public sealed class FolderGridControl : Control
{
    private const double RowHeight = 20;
    private static readonly Typeface Face = new("Segoe UI, Arial");

    private FolderCompare? _folder;
    private FolderView? _view;
    private DirRow[] _rows = new DirRow[128];
    private long _top;
    private uint _count;

    public FrameStats Stats { get; } = new();

    public int VisibleRows => Math.Max(1, (int)(Bounds.Height / RowHeight));

    public FolderView? View => _view;

    public void SetFolder(FolderCompare? folder, string? viewJson = null)
    {
        _view?.Dispose();
        _folder = folder;
        _view = folder?.CreateView(viewJson);
        _top = 0;
        Refresh();
    }

    public void Refresh()
    {
        _count = _view?.RowCount().Count ?? 0;
        InvalidateVisual();
    }

    protected override void OnPointerWheelChanged(PointerWheelEventArgs e)
    {
        _top = Math.Max(0, Math.Min(_top - (long)(e.Delta.Y * 3), Math.Max(0, _count - VisibleRows)));
        InvalidateVisual();
        e.Handled = true;
    }

    protected override void OnPointerPressed(PointerPressedEventArgs e)
    {
        if (_view is null) return;
        var row = (int)(e.GetPosition(this).Y / RowHeight);
        var n = _view.GetRows((uint)(_top + row), _rows.AsSpan(0, 1));
        if (n == 1 && _rows[0].ChildCount > 0)
        {
            var node = _rows[0].Node;
            // a folder is expanded when its first child is shown right after it: ask the view again
            var before = _view.RowCount().Count;
            _view.Expand(node, true);
            if (_view.RowCount().Count == before) _view.Expand(node, false);
            Refresh();
        }
    }

    private static IBrush Fore(NodeStatus s) => s switch
    {
        NodeStatus.Same => Brushes.LightGray,
        NodeStatus.UnimportantDiff => Brushes.Khaki,
        NodeStatus.LeftNewer or NodeStatus.RightNewer or NodeStatus.Different => Brushes.Orange,
        NodeStatus.LeftOrphan => Brushes.IndianRed,
        NodeStatus.RightOrphan => Brushes.LightGreen,
        _ => Brushes.Red,
    };

    public override void Render(DrawingContext ctx)
    {
        base.Render(ctx);
        ctx.FillRectangle(Brushes.Transparent, new Rect(Bounds.Size));
        if (_view is null || _folder is null) return;
        var visible = VisibleRows + 1;
        if (_rows.Length < visible) _rows = new DirRow[visible];
        var sw = Stopwatch.StartNew();
        var n = _view.GetRows((uint)_top, _rows.AsSpan(0, visible));
        Span<char> name = stackalloc char[256];
        var lines = new string[n];
        for (var i = 0; i < n; i++) lines[i] = _folder.GetName(_rows[i].Node);
        sw.Stop();
        Stats.Add(sw.Elapsed.TotalMilliseconds);
        for (var i = 0; i < n; i++)
        {
            ref readonly var r = ref _rows[i];
            var y = i * RowHeight;
            var mark = r.ChildCount > 0 ? "▸ " : "  ";
            var t = new FormattedText(mark + lines[i], CultureInfo.InvariantCulture, FlowDirection.LeftToRight, Face, 13, Fore(r.Status));
            ctx.DrawText(t, new Point(6 + r.Depth * 16, y + 2));
            var info = new FormattedText($"{r.Status}  L {Size(r.Left)}  R {Size(r.Right)}", CultureInfo.InvariantCulture,
                FlowDirection.LeftToRight, Face, 12, Brushes.Gray);
            ctx.DrawText(info, new Point(Math.Max(320, Bounds.Width - 320), y + 3));
        }
        _ = name;
    }

    private static string Size(in SideInfo s) => s.Present == 0 ? "-" : s.Kind == ItemKind.Folder ? "<dir>" : $"{s.Size:N0}";
}
