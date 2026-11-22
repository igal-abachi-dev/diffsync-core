//! Sync rules and presets (plan.md §8.1, decisions D10, D13; backlog E8-S1).

use core_base::{Error, Result};
use core_folder::Status;
use serde::{Deserialize, Serialize};

/// What to do with one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Op {
    Leave,
    /// Needs a decision from the user; the preview lists it with `Leave` preselected (D10).
    Prompt,
    CopyToLeft,
    CopyToRight,
    DeleteLeft,
    DeleteRight,
}

/// The five presets of the original (`TDcSyncRules.FromPreset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Preset {
    #[default]
    UpdateRight,
    UpdateLeft,
    UpdateBoth,
    MirrorToRight,
    MirrorToLeft,
}

/// All presets, in table order.
pub const PRESETS: [Preset; 5] = [
    Preset::UpdateRight,
    Preset::UpdateLeft,
    Preset::UpdateBoth,
    Preset::MirrorToRight,
    Preset::MirrorToLeft,
];

/// The sync rules. JSON keys are camelCase; missing keys take the defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncRules {
    pub preset: Preset,
    /// Deletes go to the Recycle Bin (Windows only).
    pub use_recycle_bin: bool,
    /// On `AccessDenied`, clear R/H/S on the destination and retry once (fix B5).
    pub overwrite_readonly: bool,
    /// Write through a `.dctmp~` sibling and rename (fix B4, decision D13: on by default).
    pub safe_copy: bool,
    pub copy_creation_time: bool,
    /// Treat `UnimportantDiff` as `Different`.
    pub copy_unimportant: bool,
    /// Read the copy back and compare checksums before the rename (fix B6).
    pub verify: bool,
}

impl Default for SyncRules {
    fn default() -> Self {
        SyncRules {
            preset: Preset::UpdateRight,
            use_recycle_bin: false,
            overwrite_readonly: true,
            safe_copy: true, // decision: D13
            copy_creation_time: false,
            copy_unimportant: false,
            verify: false,
        }
    }
}

impl SyncRules {
    pub fn from_preset(preset: Preset) -> Self {
        SyncRules {
            preset,
            ..Self::default()
        }
    }

    /// The operation the preset prescribes for an item in `status` (plan.md §8.1 table).
    #[allow(clippy::match_same_arms, clippy::items_after_statements)] // one arm per table cell
    pub fn op_for(&self, status: Status) -> Op {
        // `UnimportantDiff` counts as `Same` unless `copy_unimportant` turns it into `Different`
        let status = match status {
            Status::UnimportantDiff if self.copy_unimportant => Status::Different,
            Status::UnimportantDiff => Status::Same,
            s => s,
        };
        use Op::{
            CopyToLeft as CL, CopyToRight as CR, DeleteLeft as DL, DeleteRight as DR, Leave as L,
            Prompt as P,
        };
        use Preset::{MirrorToLeft, MirrorToRight, UpdateBoth, UpdateLeft, UpdateRight};
        match (status, self.preset) {
            (Status::Same | Status::Error, _) => L,
            // decision: D10: no newer side under an Update preset is a prompt, not a guess
            (Status::Different, UpdateRight | UpdateLeft | UpdateBoth) => P,
            (Status::Different, MirrorToRight) => CR,
            (Status::Different, MirrorToLeft) => CL,
            (Status::LeftNewer, UpdateRight | UpdateBoth | MirrorToRight) => CR,
            (Status::LeftNewer, UpdateLeft) => L,
            (Status::LeftNewer, MirrorToLeft) => CL,
            (Status::RightNewer, UpdateRight) => L,
            (Status::RightNewer, UpdateLeft | UpdateBoth | MirrorToLeft) => CL,
            (Status::RightNewer, MirrorToRight) => CR,
            (Status::LeftOrphan, UpdateRight | UpdateBoth | MirrorToRight) => CR,
            (Status::LeftOrphan, UpdateLeft) => L,
            (Status::LeftOrphan, MirrorToLeft) => DL,
            (Status::RightOrphan, UpdateRight) => L,
            (Status::RightOrphan, UpdateLeft | UpdateBoth | MirrorToLeft) => CL,
            (Status::RightOrphan, MirrorToRight) => DR,
            (Status::UnimportantDiff, _) => L, // unreachable after the mapping above
        }
    }

    /// Parses rules JSON; unknown keys are ignored.
    pub fn from_json(json: &str) -> Result<SyncRules> {
        serde_json::from_str(json)
            .map_err(|e| Error::invalid_argument(format!("bad sync rules: {e}")))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUSES: [Status; 8] = [
        Status::Same,
        Status::UnimportantDiff,
        Status::Different,
        Status::LeftNewer,
        Status::RightNewer,
        Status::LeftOrphan,
        Status::RightOrphan,
        Status::Error,
    ];

    /// The §8.1 table written out cell by cell (rows = status, columns = the five presets in
    /// `PRESETS` order). `P` is Prompt.
    #[test]
    fn the_preset_table_matches_plan_8_1_in_all_40_cells() {
        use Op::{
            CopyToLeft as CL, CopyToRight as CR, DeleteLeft as DL, DeleteRight as DR, Leave as L,
            Prompt as P,
        };
        let table: [(Status, [Op; 5]); 8] = [
            (Status::Same, [L, L, L, L, L]),
            (Status::UnimportantDiff, [L, L, L, L, L]),
            (Status::Different, [P, P, P, CR, CL]),
            (Status::LeftNewer, [CR, L, CR, CR, CL]),
            (Status::RightNewer, [L, CL, CL, CR, CL]),
            (Status::LeftOrphan, [CR, L, CR, CR, DL]),
            (Status::RightOrphan, [L, CL, CL, DR, CL]),
            (Status::Error, [L, L, L, L, L]),
        ];
        let mut cells = 0;
        for (status, row) in table {
            for (preset, want) in PRESETS.into_iter().zip(row) {
                assert_eq!(
                    SyncRules::from_preset(preset).op_for(status),
                    want,
                    "{preset:?} / {status:?}"
                );
                cells += 1;
            }
        }
        assert_eq!(cells, 40);
        assert_eq!(STATUSES.len() * PRESETS.len(), 40);
    }

    #[test]
    fn copy_unimportant_turns_unimportant_into_different() {
        let mut r = SyncRules::from_preset(Preset::MirrorToRight);
        assert_eq!(r.op_for(Status::UnimportantDiff), Op::Leave);
        r.copy_unimportant = true;
        assert_eq!(r.op_for(Status::UnimportantDiff), Op::CopyToRight);
        let mut u = SyncRules::from_preset(Preset::UpdateBoth);
        u.copy_unimportant = true;
        assert_eq!(u.op_for(Status::UnimportantDiff), Op::Prompt);
    }

    #[test]
    fn defaults_match_the_plan() {
        let d = SyncRules::default();
        assert!(
            !d.use_recycle_bin
                && d.overwrite_readonly
                && !d.copy_creation_time
                && !d.copy_unimportant
        );
        assert!(d.safe_copy, "decision D13: safe copy is on by default");
        assert!(!d.verify);
    }

    #[test]
    fn json_round_trip_and_defaults() {
        let r =
            SyncRules::from_json(r#"{"preset":"mirrorToLeft","useRecycleBin":true,"future":1}"#)
                .unwrap();
        assert_eq!(r.preset, Preset::MirrorToLeft);
        assert!(r.use_recycle_bin && r.safe_copy);
        assert_eq!(SyncRules::from_json(&r.to_json()).unwrap(), r);
        assert_eq!(SyncRules::from_json("{}").unwrap(), SyncRules::default());
        assert!(SyncRules::from_json(r#"{"preset":"nope"}"#).is_err());
    }
}
