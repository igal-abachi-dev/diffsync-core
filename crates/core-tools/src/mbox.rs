//! A diff as a git mailbox patch, the mail `git format-patch` writes and `git am` reads (backlog
//! E11.5-S16): the commit message and author, the diffstat, the patch and the version trailer.

use core_diff::patch::encode_header_words;

/// What goes into the mail besides the patch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailMeta {
    /// The one-line summary (`[PATCH] <subject>`). Empty: `Update`.
    pub subject: String,
    /// `Name <address>`. Empty: `DiffSync <diffsync@localhost>`.
    pub author: String,
    /// RFC 2822 date. Empty: the time of the call.
    pub date: String,
    /// The commit message below the subject; may be empty.
    pub message: String,
}

/// The mail for `patch` (git-style unified diff text) with `stat` (a `--stat` table) below the
/// `---` line. Both may be empty only together: a mail without changes is not written.
pub fn render_mbox(meta: &MailMeta, stat: &str, patch: &str, date_now: &str) -> String {
    if patch.is_empty() {
        return String::new();
    }
    let subject = if meta.subject.trim().is_empty() {
        "Update"
    } else {
        meta.subject.trim()
    };
    let author = if meta.author.trim().is_empty() {
        "DiffSync <diffsync@localhost>"
    } else {
        meta.author.trim()
    };
    let date = if meta.date.trim().is_empty() {
        date_now
    } else {
        meta.date.trim()
    };
    let mut s = String::new();
    // git writes the commit id here; there is no commit, and `git am` ignores the value
    s.push_str("From 0000000000000000000000000000000000000000 Mon Sep 17 00:00:00 2001\n");
    s.push_str(&format!("From: {}\n", encode_header_words(author)));
    s.push_str(&format!("Date: {date}\n"));
    s.push_str(&format!(
        "Subject: [PATCH] {}\n\n",
        encode_header_words(&subject.replace('\n', " "))
    ));
    if !meta.message.trim().is_empty() {
        s.push_str(meta.message.trim_end());
        s.push('\n');
    }
    s.push_str("---\n");
    s.push_str(stat);
    if !stat.is_empty() && !stat.ends_with('\n') {
        s.push('\n');
    }
    s.push('\n');
    s.push_str(patch);
    if !patch.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(&format!(
        "-- \ndiffsync-core {}\n\n",
        env!("CARGO_PKG_VERSION")
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_diff::patch::parse_patches;

    #[test]
    fn the_mail_has_git_s_shape_and_reads_back() {
        let patch = "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let m = render_mbox(
            &MailMeta {
                subject: "Fix é".into(),
                author: "Ann <ann@example.com>".into(),
                date: "Tue, 1 Jan 2030 10:00:00 +0000".into(),
                message: "Why.".into(),
            },
            " f.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n",
            patch,
            "now",
        );
        assert!(m.starts_with("From 0000000000000000000000000000000000000000 Mon Sep 17 00:00:00 2001\nFrom: Ann <ann@example.com>\nDate: Tue, 1 Jan 2030 10:00:00 +0000\nSubject: [PATCH] =?UTF-8?B?"), "{m}");
        assert!(m.contains("\n\nWhy.\n---\n f.txt | 2 +-\n"), "{m}");
        assert!(m.ends_with("-- \ndiffsync-core 0.1.0\n\n") || m.contains("-- \ndiffsync-core "));
        let p = parse_patches(&m).unwrap();
        let h = p[0].mail.as_ref().unwrap();
        assert_eq!((h.subject.as_str(), h.message.as_str()), ("Fix é", "Why."));
        assert_eq!(p[0].files[0].hunks[0].lines.len(), 2);
        assert_eq!(render_mbox(&MailMeta::default(), "", "", "now"), "");
    }
}
