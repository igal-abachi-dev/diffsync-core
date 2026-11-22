// Runs every pattern of a file through the C# Glob library over a folder and prints
// "pattern<TAB>result1|result2|..." with results relative to the folder, sorted.
// usage: GlobDiffRunner <baseDir> <patternsFile>
using System;
using System.IO;
using System.Linq;
using Globbing;

internal static class Program
{
    private static int Main(string[] args)
    {
        if (args.Length > 0 && args[0] == "--") args = args.Skip(1).ToArray();
        var baseDir = Path.GetFullPath(args[0]);
        foreach (var pattern in File.ReadAllLines(args[1]).Where(l => l.Length > 0))
        {
            string result;
            try
            {
                var options = new GlobOptions
                {
                    BaseDirectory = baseDir,
                    CaseSensitive = true,
                    Dot = true,
                    WindowsPathsNoEscape = true,
                    IncludeDirectories = false,
                    MatchBase = true,
                };
                result = string.Join("|", Glob.Match(pattern, options)
                    .Select(p => p.Replace((char)92, (char)47)).OrderBy(p => p, StringComparer.Ordinal));
            }
            catch (Exception e)
            {
                result = "ERROR:" + e.GetType().Name;
            }
            Console.WriteLine(pattern + "\t" + result);
        }
        return 0;
    }
}
