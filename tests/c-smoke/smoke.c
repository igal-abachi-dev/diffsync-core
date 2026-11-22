/* C smoke test for diffsync_core (backlog E9-S8): struct layout, open -> pull -> close for text
 * and folder, a stale handle, a too-small buffer, the panic barrier and a sync dry run.
 * Usage: smoke <work dir>   (the test harness creates left.txt, right.txt, L\ and R\ in it)
 * Windows only: UTF-16 strings are wchar_t. */
#define _CRT_SECURE_NO_WARNINGS
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>

#include "diffsync.h"

/* exported only with the `test-panic` feature: not in the header or the C# bindings */
DsStatus ds_test_panic(void);

#define CHECK(cond)                                                                        \
    do {                                                                                   \
        if (!(cond)) {                                                                     \
            fprintf(stderr, "FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond);              \
            return 1;                                                                      \
        }                                                                                  \
    } while (0)

/* struct sizes and offsets are part of the ABI (plan.md 10.3) */
_Static_assert(sizeof(DsTextRow) == 16, "DsTextRow");
_Static_assert(sizeof(DsRange) == 12, "DsRange");
_Static_assert(sizeof(DsSection) == 12, "DsSection");
_Static_assert(sizeof(DsFold) == 8, "DsFold");
_Static_assert(sizeof(DsFindHit) == 20, "DsFindHit");
_Static_assert(sizeof(DsVolumeInfo) == 16, "DsVolumeInfo");
_Static_assert(sizeof(DsSideInfo) == 32, "DsSideInfo");
_Static_assert(sizeof(DsDirRow) == 88, "DsDirRow");
_Static_assert(sizeof(DsPlanAction) == 16, "DsPlanAction");
_Static_assert(sizeof(DsQuestion) == 16, "DsQuestion");
_Static_assert(offsetof(DsTextRow, left_line) == 0, "left_line");
_Static_assert(offsetof(DsTextRow, right_line) == 4, "right_line");
_Static_assert(offsetof(DsTextRow, kind) == 8, "kind");
_Static_assert(offsetof(DsTextRow, section) == 12, "section");
_Static_assert(offsetof(DsDirRow, left) == 16, "DsDirRow.left");
_Static_assert(offsetof(DsDirRow, right) == 48, "DsDirRow.right");
_Static_assert(offsetof(DsDirRow, diff_count) == 80, "DsDirRow.diff_count");
_Static_assert(offsetof(DsSideInfo, attrs) == 24, "DsSideInfo.attrs");
_Static_assert(sizeof(DsTextStatus) == 56, "DsTextStatus");
_Static_assert(sizeof(DsDocInfo) == 72, "DsDocInfo");
_Static_assert(sizeof(DsFolderStatus) == 72, "DsFolderStatus");
_Static_assert(sizeof(DsPlanSummary) == 56, "DsPlanSummary");
_Static_assert(sizeof(DsJobStatus) == 64, "DsJobStatus");
_Static_assert(sizeof(DsLineInfo) == 8, "DsLineInfo");
_Static_assert(offsetof(DsTextStatus, generation) == 8, "DsTextStatus.generation");
_Static_assert(offsetof(DsFolderStatus, revision) == 64, "DsFolderStatus.revision");
_Static_assert(offsetof(DsJobStatus, bytes_copied) == 48, "DsJobStatus.bytes_copied");
_Static_assert(sizeof(DsInitOptions) == 56, "DsInitOptions (64-bit)");

static void join(wchar_t *out, size_t cap, const wchar_t *dir, const wchar_t *name) {
    _snwprintf(out, cap, L"%ls\\%ls", dir, name);
}

static DsStr16 view16(const wchar_t *s) {
    DsStr16 v;
    v.ptr = (const uint16_t *)s;
    v.len = wcslen(s);
    return v;
}

static DsStr8 view8(const char *s) {
    DsStr8 v;
    v.ptr = (const uint8_t *)s;
    v.len = strlen(s);
    return v;
}

int wmain(int argc, wchar_t **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: smoke <work dir>\n");
        return 2;
    }
    const wchar_t *dir = argv[1];
    wchar_t left[512], right[512], lroot[512], rroot[512];
    join(left, 512, dir, L"left.txt");
    join(right, 512, dir, L"right.txt");
    join(lroot, 512, dir, L"L");
    join(rroot, 512, dir, L"R");

    /* version */
    CHECK((ds_abi_version() >> 16) == ABI_MAJOR);
    CHECK((ds_abi_version() & 0xFFFF) == ABI_MINOR);

    /* text compare: open -> pull -> close */
    DsHandle t = 0;
    CHECK(ds_text_open(view16(left), view16(right), view8(""), NULL, NULL, &t) == DS_OK);
    DsTextStatus st;
    for (int i = 0; i < 5000; i++) {
        CHECK(ds_text_status(t, &st) == DS_OK);
        if (st.state == 2) break;
        Sleep(2);
    }
    if (st.state != 2) {
        char why[256];
        size_t n = 0;
        ds_last_error((uint8_t *)why, sizeof why - 1, &n);
        why[n < sizeof why - 1 ? n : sizeof why - 1] = 0;
        fprintf(stderr, "text state %u: %s\n", st.state, why);
    }
    CHECK(st.state == 2);
    CHECK(st.rows == 4);
    CHECK(st.important_diff == 1);
    DsTextRow rows[8];
    uint32_t written = 0;
    CHECK(ds_text_get_rows(t, 0, 8, rows, &written) == DS_OK);
    CHECK(written == 4);
    CHECK(rows[1].kind == 2 && rows[1].left_line == 1 && rows[1].right_line == 1);
    CHECK((rows[1].flags & 1) == 1);

    /* buffer too small: the size comes back */
    uint32_t len = 0;
    CHECK(ds_text_get_line(t, 0, 1, NULL, 0, &len, NULL) == DS_E_BUFFER_TOO_SMALL);
    CHECK(len == 5); /* "beta1" */
    uint16_t line[16];
    CHECK(ds_text_get_line(t, 0, 1, line, 16, &len, NULL) == DS_OK);
    CHECK(len == 5 && line[0] == 'b');

    /* close, then the stale handle */
    CHECK(ds_close(t) == DS_OK);
    CHECK(ds_text_status(t, &st) == DS_E_INVALID_HANDLE);
    CHECK(ds_close(t) == DS_E_INVALID_HANDLE);

    /* the panic barrier */
    CHECK(ds_test_panic() == DS_E_INTERNAL);
    char msg[256];
    size_t needed = 0;
    CHECK(ds_last_error((uint8_t *)msg, sizeof msg, &needed) == DS_OK);
    CHECK(needed > 0);

    /* folder compare + view */
    DsHandle f = 0;
    CHECK(ds_folder_open(view16(lroot), view16(rroot), view8(""), NULL, NULL, &f) == DS_OK);
    DsFolderStatus fs;
    for (int i = 0; i < 5000; i++) {
        CHECK(ds_folder_status(f, &fs) == DS_OK);
        if (fs.state != 0) break;
        Sleep(2);
    }
    CHECK(fs.state == 1);
    CHECK(fs.left_orphan == 1 && fs.right_orphan == 1);
    DsHandle v = 0;
    CHECK(ds_view_create(f, view8(""), &v) == DS_OK);
    uint32_t count = 0;
    uint64_t generation = 0;
    CHECK(ds_view_row_count(v, &count, &generation) == DS_OK);
    CHECK(count == 3);
    DsDirRow dirs[4];
    CHECK(ds_view_get_rows(v, 0, 4, dirs, &written) == DS_OK);
    CHECK(written == 3);

    /* sync dry run: plan only, nothing is executed */
    DsHandle plan = 0;
    CHECK(ds_sync_plan(f, view8("{\"preset\":\"mirrorToRight\"}"), NULL, 0, &plan) == DS_OK);
    DsPlanSummary sum;
    CHECK(ds_plan_summary(plan, &sum) == DS_OK);
    CHECK(sum.copies_to_right == 1 && sum.deletes_right == 1);
    DsBuf report = {0};
    CHECK(ds_plan_report(plan, &report) == DS_OK);
    CHECK(report.len > 0 && strstr((const char *)report.ptr, "copy-to-right") != NULL);
    ds_buf_free(report);
    CHECK(ds_close(plan) == DS_OK);
    CHECK(ds_close(v) == DS_OK);
    CHECK(ds_close(f) == DS_OK);

    printf("c smoke test passed\n");
    return 0;
}
