// The test application window: paths, buttons, the text compare control, the folder grid, and a
// question dialog for the sync handshake. Not the product UI.
using System;
using System.Threading.Tasks;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using DiffSync.Native;

namespace DiffSync.TestApp;

public sealed class MainWindow : Window
{
    private readonly TextBox _left = new() { Watermark = "left file or folder", MinWidth = 320 };
    private readonly TextBox _right = new() { Watermark = "right file or folder", MinWidth = 320 };
    private readonly TextCompareControl _text = new() { Focusable = true };
    private readonly ScrollBar _scroll = new() { Orientation = Orientation.Vertical, Width = 16 };
    private readonly FolderGridControl _grid = new();
    private readonly TextBlock _status = new() { Margin = new Thickness(8, 4) };

    private TextCompare? _tc;
    private FolderCompare? _folder;
    private SyncPlan? _plan;
    private Job? _job;

    public MainWindow()
    {
        Title = "diffsync test app (ABI " + DiffSyncLibrary.AbiMajor + "." + DiffSyncLibrary.AbiMinor + ")";
        Width = 1200;
        Height = 800;

        var bar = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, Margin = new Thickness(6) };
        bar.Children.Add(_left);
        bar.Children.Add(_right);
        bar.Children.Add(Button("Compare text", (_, _) => OpenText()));
        bar.Children.Add(Button("Compare folders", (_, _) => OpenFolder()));
        bar.Children.Add(Button("Differences only", (_, _) => _grid.View?.Set("{\"filter\":\"differences\"}")));
        bar.Children.Add(Button("All", (_, _) => _grid.View?.Set("{\"filter\":\"all\"}")));
        bar.Children.Add(Button("Plan: mirror to right (dry run)", (_, _) => MakePlan()));
        bar.Children.Add(Button("Execute plan", (_, _) => Execute()));

        var textArea = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto") };
        textArea.Children.Add(_text);
        Grid.SetColumn(_scroll, 1);
        textArea.Children.Add(_scroll);
        _scroll.ValueChanged += (_, e) => _text.TopRow = (long)e.NewValue;
        _text.TopChanged += top => { if ((long)_scroll.Value != top) _scroll.Value = top; };

        var body = new Grid { RowDefinitions = new RowDefinitions("*,5,*") };
        body.Children.Add(textArea);
        var split = new Border { Background = Brushes.Gray };
        Grid.SetRow(split, 1);
        body.Children.Add(split);
        Grid.SetRow(_grid, 2);
        body.Children.Add(_grid);

        var dock = new DockPanel();
        DockPanel.SetDock(bar, Dock.Top);
        DockPanel.SetDock(_status, Dock.Bottom);
        dock.Children.Add(bar);
        dock.Children.Add(_status);
        dock.Children.Add(body);
        Content = dock;

        var timer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(250) };
        timer.Tick += (_, _) => Tick();
        timer.Start();
    }

    private static Button Button(string text, EventHandler<Avalonia.Interactivity.RoutedEventArgs> click)
    {
        var b = new Button { Content = text };
        b.Click += click;
        return b;
    }

    private void Tick()
    {
        var parts = new System.Collections.Generic.List<string>();
        if (_tc is not null)
        {
            var s = _tc.Status;
            _text.Refresh();
            _scroll.Maximum = Math.Max(0, s.Rows - _text.VisibleRows);
            parts.Add($"text {s.State}, {s.Rows:N0} rows, {s.ImportantDiff} important; {_text.Stats}");
        }
        if (_folder is not null)
        {
            var f = _folder.Status;
            parts.Add($"folder {f.State}, {f.NodeCount:N0} nodes; {_grid.Stats}");
        }
        if (_job is not null)
        {
            var j = _job.Status;
            parts.Add($"job {j.State} {j.StepsDone}/{j.StepsTotal}, done {j.Done}, skipped {j.Skipped}, failed {j.Failed}");
        }
        _status.Text = string.Join("   |   ", parts);
    }

    private void Guard(Action a)
    {
        try { a(); }
        catch (DiffSyncException e) { _status.Text = $"{e.Kind}: {e.Message}"; }
    }

    private void OpenText() => Guard(() =>
    {
        _tc?.Dispose();
        _tc = TextCompare.Open(_left.Text ?? "", _right.Text ?? "", null, null, a => Dispatcher.UIThread.Post(a));
        _text.SetCompare(_tc);
        _text.Focus();
    });

    private void OpenFolder() => Guard(() =>
    {
        _plan?.Dispose();
        _plan = null;
        _grid.SetFolder(null);
        _folder?.Dispose();
        _folder = FolderCompare.Open(_left.Text ?? "", _right.Text ?? "", null, e =>
        {
            if (e.Kind is EventKind.State or EventKind.Node) _grid.Refresh();
        }, a => Dispatcher.UIThread.Post(a));
        // the view needs a finished compare: wait for it without blocking the UI thread
        var f = _folder;
        DispatcherTimer.RunOnce(() => TryCreateView(f, 0), TimeSpan.FromMilliseconds(100));
    });

    private void TryCreateView(FolderCompare f, int attempt)
    {
        if (!ReferenceEquals(f, _folder)) return;
        var st = f.Status;
        if (st.State == FolderState.Scanning && attempt < 600)
        {
            DispatcherTimer.RunOnce(() => TryCreateView(f, attempt + 1), TimeSpan.FromMilliseconds(100));
            return;
        }
        if (st.State == FolderState.Done) Guard(() => _grid.SetFolder(f));
        else _status.Text = "folder compare " + st.State + ": " + f.FailureMessage;
    }

    private void MakePlan() => Guard(() =>
    {
        if (_folder is null) { _status.Text = "compare folders first"; return; }
        _plan?.Dispose();
        _plan = _folder.Plan("{\"preset\":\"mirrorToRight\"}");
        var s = _plan.Summary;
        _status.Text = $"dry run: {s.CopiesToRight} copies ({s.BytesToRight:N0} bytes), {s.FoldersCreated} folders, " +
                       $"{s.DeletesRight} deletes, {s.Conflicts} prompts, {s.Errors} errors, {s.SkippedLinks} links skipped";
    });

    private void Execute() => Guard(() =>
    {
        if (_plan is null) { _status.Text = "make a plan first"; return; }
        _job?.Dispose();
        _job = _plan.Execute(null, e =>
        {
            if (e.Kind == EventKind.Question) _ = AskAsync();
            if (e.Kind == EventKind.Node) _grid.Refresh();
        }, a => Dispatcher.UIThread.Post(a));
    });

    /// <summary>The question handshake: show a dialog, answer through the job API.</summary>
    private async Task AskAsync()
    {
        var job = _job;
        if (job?.GetQuestion() is not { } q) return;
        var answer = await new QuestionWindow(q).ShowDialog<Answer?>(this) ?? Answer.Skip;
        try { job.Answer(q.Id, answer); } catch (DiffSyncException) { /* the question was answered elsewhere */ }
    }

    protected override void OnClosed(EventArgs e)
    {
        _job?.Dispose();
        _plan?.Dispose();
        _grid.SetFolder(null);
        _folder?.Dispose();
        _tc?.Dispose();
        base.OnClosed(e);
    }
}

/// <summary>"The file changed since the compare": Retry / Skip / Skip all / Abort.</summary>
public sealed class QuestionWindow : Window
{
    public QuestionWindow(Question q)
    {
        Title = q.Kind == QuestionKind.ChangedSinceCompare ? "Changed since the compare" : "Action failed";
        Width = 560;
        Height = 200;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        foreach (var (text, answer) in new[] { ("Retry", Answer.Retry), ("Skip", Answer.Skip), ("Skip all", Answer.SkipAll), ("Abort", Answer.Abort) })
        {
            var b = new Button { Content = text };
            b.Click += (_, _) => Close(answer);
            buttons.Children.Add(b);
        }
        var panel = new DockPanel { Margin = new Thickness(12) };
        DockPanel.SetDock(buttons, Dock.Bottom);
        panel.Children.Add(buttons);
        panel.Children.Add(new TextBlock { Text = q.Message, TextWrapping = TextWrapping.Wrap });
        Content = panel;
    }
}

public sealed class App : Application
{
    public override void Initialize() => Styles.Add(new Avalonia.Themes.Fluent.FluentTheme());

    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is Avalonia.Controls.ApplicationLifetimes.IClassicDesktopStyleApplicationLifetime d)
        {
            d.MainWindow = new MainWindow();
        }
        base.OnFrameworkInitializationCompleted();
    }
}
