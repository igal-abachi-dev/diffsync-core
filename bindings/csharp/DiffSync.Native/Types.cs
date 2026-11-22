// Public, blittable mirrors of the ABI structs (docs/abi.md, "Struct layouts"). Their layout is
// identical to the generated Ds* structs, so a pooled array of these is filled by the core with no
// copy and no marshalling.
using System;
using System.Runtime.InteropServices;

namespace DiffSync.Native;

/// <summary>What a text row is. Values are part of the ABI.</summary>
public enum RowKind : byte
{
    Same = 0,
    Unimportant = 1,
    Different = 2,
    LeftOnly = 3,
    RightOnly = 4,
    UnimportantLeftOnly = 5,
    UnimportantRightOnly = 6,
}

[StructLayout(LayoutKind.Sequential)]
public struct TextRow
{
    public const uint Absent = uint.MaxValue;
    public uint LeftLine;
    public uint RightLine;
    public RowKind Kind;
    /// <summary>bit 0: first row of a section; bit 1: important difference.</summary>
    public byte Flags;
    public ushort Reserved;
    /// <summary>Section index, <see cref="uint.MaxValue"/> for equal rows.</summary>
    public uint Section;

    public readonly bool StartsSection => (Flags & 1) != 0;
    public readonly bool IsImportant => (Flags & 2) != 0;
}

[StructLayout(LayoutKind.Sequential)]
public struct TextRange
{
    /// <summary>UTF-16 units.</summary>
    public uint Start;
    public uint Length;
    /// <summary>1 = important difference.</summary>
    public byte Kind;
    private byte _p0, _p1, _p2;
}

[StructLayout(LayoutKind.Sequential)]
public struct Section
{
    public uint FirstRow;
    public uint RowCount;
    public byte Important;
    private byte _p0, _p1, _p2;
}

public enum LineEnding : byte { None = 0, CrLf = 1, Lf = 2, Cr = 3, Unknown = 255 }

/// <summary>The state of a text compare.</summary>
public enum TextState : uint { Loading = 0, Aligning = 1, Done = 2, Failed = 3, Cancelled = 4, Binary = 5 }

public readonly record struct TextStatus(
    TextState State, uint Rows, ulong Generation,
    uint Same, uint ImportantDiff, uint UnimportantDiff, uint LeftOrphan, uint RightOrphan,
    uint UnimportantLeftOrphan, uint UnimportantRightOrphan, uint Sections, uint ImportantSections);

public readonly record struct DocInfo(
    uint LineCount, bool IsBinary, bool HasBom, ulong ByteLength, ulong[] EndingCounts, string Encoding);

public enum NodeStatus : byte
{
    Same = 0, UnimportantDiff = 1, Different = 2, LeftNewer = 3, RightNewer = 4,
    LeftOrphan = 5, RightOrphan = 6, Error = 7,
}

[System.Flags]
public enum NodeFlags : byte
{
    None = 0, PendingContent = 1, ContentCompared = 2, TypeConflict = 4, DuplicateKey = 8,
    Offline = 16, Link = 32, FilteredChildren = 64, Gone = 128,
}

public enum ItemKind : byte { File = 0, Folder = 1, Link = 2 }

[StructLayout(LayoutKind.Sequential)]
public struct SideInfo
{
    public ulong Size;
    /// <summary>FILETIME ticks (100 ns since 1601).</summary>
    public long MTime;
    public long CTime;
    public uint Attributes;
    public byte Present;
    public ItemKind Kind;
    public ushort Reserved;
}

/// <summary>One row of the folder view (and the info of a node).</summary>
[StructLayout(LayoutKind.Sequential)]
public struct DirRow
{
    public uint Node;
    public uint Parent;
    public ushort Depth;
    public NodeStatus Status;
    public NodeFlags Flags;
    public uint ChildCount;
    public SideInfo Left;
    public SideInfo Right;
    public uint DiffCount;
    public uint OrphanCount;
}

public enum FolderState : uint { Scanning = 0, Done = 1, Failed = 2, Cancelled = 3 }

public readonly record struct FolderStatus(
    FolderState State, uint Stage, ulong ProgressDone, ulong ProgressTotal, uint NodeCount,
    uint WarningCount, bool SyncBlocked, uint Diff, uint LeftNewer, uint RightNewer,
    uint LeftOrphan, uint RightOrphan, uint Error, uint Items, ulong Revision);

public readonly record struct PlanSummary(
    uint CopiesToLeft, uint CopiesToRight, uint FoldersCreated, uint DeletesLeft, uint DeletesRight,
    uint Conflicts, uint Errors, uint SkippedLinks, ulong BytesToLeft, ulong BytesToRight,
    uint ActionCount, bool FiltersActive);

public enum ActionKind : byte
{
    CreateFolderLeft = 0, CreateFolderRight = 1, CopyToLeft = 2, CopyToRight = 3, DeleteLeft = 4, DeleteRight = 5,
}

[StructLayout(LayoutKind.Sequential)]
public struct PlanAction
{
    public ActionKind Kind;
    public byte IsFolder;
    /// <summary>bit 0: delete only if empty; bit 1: frees a name for a type-conflict copy.</summary>
    public ushort Flags;
    public uint Node;
    public ulong Bytes;
}

/// <summary>What to do with a node (<c>ds_plan_set_op</c>).</summary>
public enum SyncOp : byte { Leave = 0, Prompt = 1, CopyToLeft = 2, CopyToRight = 3, DeleteLeft = 4, DeleteRight = 5 }

public enum JobState : uint { Running = 0, Paused = 1, Finished = 2 }

public readonly record struct JobStatus(
    JobState State, uint StepsTotal, uint StepsDone, bool HasQuestion, ulong BytesTotal, ulong BytesDone,
    uint Done, uint Skipped, uint Failed, uint Kept, ulong BytesCopied, bool Aborted, bool Cancelled);

public enum QuestionKind : uint { ChangedSinceCompare = 0, Failed = 1 }

public readonly record struct Question(ulong Id, QuestionKind Kind, uint Step, string Message);

/// <summary>The answer to a question (<c>ds_job_answer</c>).</summary>
public enum Answer : byte { Retry = 0, Skip = 1, SkipAll = 2, Abort = 3 }

/// <summary>A run of rows the Context view hides ("36 filtered lines").</summary>
public readonly record struct Fold(uint FirstRow, uint RowCount);

/// <summary>What a difference section does (Beyond Compare's insert, delete and replacement).</summary>
public enum SectionKind : byte { Insert = 0, Delete = 1, Replace = 2 }

/// <summary>A match of <see cref="TextCompare.Find"/>: the row, the side and line, and the range in UTF-16 units.</summary>
public readonly record struct FindHit(uint Row, bool OnRightSide, uint Line, uint Start, uint End);

[Flags]
public enum FindFlags : uint { None = 0, CaseSensitive = 1, WholeWord = 2, Regex = 4, SkipLeft = 8, SkipRight = 16 }
