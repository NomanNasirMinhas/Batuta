//! The wizard's step machine.
//!
//! Pure: it asks questions through [`Ask`] and returns a [`SetupPlan`]. It
//! touches nothing on the machine, which is what lets the whole branching
//! structure be tested as data.
//!
//! Every answer is collected before anything is applied. That is not just
//! politeness — installing the service at step 1 would start a daemon before
//! steps 2 to 4 have said which drives to index or where to put the data, so
//! it would come up misconfigured and need fixing moments later. Collecting
//! first also means a user who backs out at the summary leaves nothing behind.

use std::path::PathBuf;

use super::drives::DriveInfo;
use super::prompt::Ask;
use crate::config::Config;

/// How the index is kept up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resident {
    /// A Windows service following the change journal in real time.
    Service,
    /// A scheduled full rescan every `minutes`.
    Task { minutes: u32 },
    /// Nothing; the index is only refreshed by running `batuta scan`.
    Manual,
}

/// Hotkey combinations the wizard offers.
pub const HOTKEYS: [&str; 3] = ["Ctrl+Space", "Ctrl+Alt+Space", "Alt+Space"];

/// Everything the user chose. No side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPlan {
    pub resident: Resident,
    pub drives: Vec<char>,
    pub exclude_system: bool,
    pub install_dir: PathBuf,
    pub data_dir: PathBuf,
    pub restrict_index: bool,
    pub keep_in_ram: bool,
    pub hotkey: Option<String>,
    pub add_to_path: bool,
}

impl SetupPlan {
    /// Fold the plan into the config that will be written.
    pub fn to_config(&self, base: &Config) -> Config {
        Config {
            drives: self.drives.clone(),
            exclude: if self.exclude_system {
                Config::default_exclusions()
            } else {
                Vec::new()
            },
            include_metadata: base.include_metadata,
            data_dir: self.data_dir.clone(),
            install_dir: self.install_dir.clone(),
            hotkey: self.hotkey.clone(),
            owner_sid: base.owner_sid.clone(),
            keep_in_ram: self.keep_in_ram,
            restrict_index: self.restrict_index,
        }
    }

    /// What the user is shown before confirming.
    ///
    /// Rendered from the same struct `apply` consumes, so the summary cannot
    /// drift from what actually happens.
    pub fn summary(&self) -> String {
        let mut out = String::from("This will:\n");
        let drives: Vec<String> = self.drives.iter().map(|d| format!("{d}:")).collect();

        out.push_str(&format!(
            "  • install batuta.exe to {}\n",
            self.install_dir.display()
        ));
        out.push_str(&format!("  • index {}\n", drives.join(", ")));
        out.push_str(if self.exclude_system {
            "  • skip Windows, Program Files and Program Files (x86)\n"
        } else {
            "  • index system directories too\n"
        });
        out.push_str(&format!(
            "  • store the index in {}\n",
            self.data_dir.display()
        ));
        if self.restrict_index {
            out.push_str("  • restrict that folder to you, Administrators and SYSTEM\n");
        } else {
            out.push_str("  • leave that folder readable by all local users\n");
        }
        out.push_str("  • build the index now\n");
        out.push_str(if self.keep_in_ram {
            "  • keep the index in memory for instant results\n"
        } else {
            "  • release the index from memory when idle, reloading on demand\n"
        });

        match &self.resident {
            Resident::Service => {
                out.push_str("  • install and start the Batuta service (real-time updates)\n")
            }
            Resident::Task { minutes } => out.push_str(&format!(
                "  • add a scheduled task re-indexing every {minutes} minute(s)\n"
            )),
            Resident::Manual => {
                out.push_str("  • not install anything resident; run `batuta scan` to refresh\n")
            }
        }
        if let Some(k) = &self.hotkey {
            out.push_str(&format!("  • start a helper so {k} opens the search bar\n"));
        }
        if self.add_to_path {
            out.push_str("  • add batuta to your PATH\n");
        }
        out
    }
}

/// Run the six steps and return what the user chose.
pub fn run<A: Ask + ?Sized>(
    ask: &mut A,
    drives: &[DriveInfo],
    base: &Config,
) -> std::io::Result<SetupPlan> {
    ask.say("Batuta Quick Setup");
    ask.say("Nothing is changed until you confirm at the end.");

    // ---- 1 / 1b / 1b-a -------------------------------------------------
    let resident = if ask.confirm(
        "Install the daemon as a service for real-time indexing?",
        true,
    )? {
        Resident::Service
    } else if ask.confirm(
        "Add a scheduled task to re-index periodically instead?",
        false,
    )? {
        let minutes = ask.number("How often, in minutes? (1-60)", 1..=60, 15)?;
        if minutes < 5 {
            ask.say(&format!(
                "  note: every {minutes} minute(s) rewrites the whole index file each time."
            ));
            ask.say("  the service updates continuously and costs far less.");
        }
        Resident::Task { minutes }
    } else {
        Resident::Manual
    };

    // ---- 2 drives ------------------------------------------------------
    let items: Vec<String> = drives.iter().map(|d| d.describe()).collect();
    let selectable: Vec<bool> = drives.iter().map(|d| d.selectable()).collect();
    let default: Vec<usize> = drives
        .iter()
        .enumerate()
        .filter(|(_, d)| d.selectable())
        .map(|(i, _)| i)
        .collect();

    let picked = ask.multi_select(
        "Which drives should be indexed?",
        &items,
        &selectable,
        &default,
    )?;
    let chosen: Vec<char> = picked.iter().map(|&i| drives[i].letter).collect();

    // ---- 3 exclusions --------------------------------------------------
    let exclude_system = ask.confirm(
        "Exclude system folders (Windows, Program Files, Program Files (x86))?",
        true,
    )?;

    // ---- 4 data location -----------------------------------------------
    let default_data = base.data_dir.display().to_string();
    let data_dir = PathBuf::from(ask.text(
        "Where should the index be stored? (press Enter for the default)",
        &default_data,
    )?);

    // ---- 4b permissions ------------------------------------------------
    ask.say("The index lists every file belonging to every user on this PC.");
    let restrict_index = ask.confirm(
        "Restrict that folder to you, Administrators and SYSTEM?",
        true,
    )?;
    if !restrict_index {
        ask.say("  note: any local account will be able to read your full file listing.");
    }

    // ---- 4c keep resident ----------------------------------------------
    // Only worth asking when a daemon will exist to hold it. Without one
    // nothing is resident either way, and the question would be meaningless.
    let keep_in_ram = if resident == Resident::Service {
        ask.say("");
        ask.say("The index is held in memory while the service runs, which is what");
        ask.say("makes searches instant. It is around 500 MB on a PC this size.");
        let keep = ask.confirm("Keep it in memory for fastest results?", true)?;
        if !keep {
            ask.say("  the service will release it when idle; the first search after");
            ask.say("  a quiet spell will then take a moment while it loads back in.");
        }
        keep
    } else {
        true
    };

    // ---- 5 hotkey ------------------------------------------------------
    let hotkey = if ask.confirm("Open the search bar with a keyboard shortcut?", true)? {
        ask.say("  Ctrl+Space is autocomplete in VS Code, Visual Studio and IntelliJ,");
        ask.say("  and switches input method on some systems. Batuta would take it.");
        let i = ask.choose("  Which shortcut?", &HOTKEYS, 0)?;
        Some(HOTKEYS[i].to_string())
    } else {
        None
    };

    // ---- 6 PATH --------------------------------------------------------
    let add_to_path = ask.confirm(
        "Add batuta to your PATH so you can run it from any terminal?",
        true,
    )?;

    Ok(SetupPlan {
        resident,
        drives: chosen,
        exclude_system,
        install_dir: base.install_dir.clone(),
        data_dir,
        restrict_index,
        keep_in_ram,
        hotkey,
        add_to_path,
    })
}

/// Warn if the chosen data directory looks unusable before anything is built.
///
/// Catching this at prompt time matters: a path the service cannot reach would
/// otherwise fail during the first checkpoint, long after setup reported success.
pub fn check_data_dir(dir: &std::path::Path, needs_system: bool) -> Option<String> {
    if !dir.is_absolute() {
        return Some(format!("{} is not an absolute path", dir.display()));
    }
    // `Path::starts_with` compares whole components, so it never matches a
    // bare separator prefix; a UNC path has to be detected on the string.
    if dir.to_string_lossy().starts_with(r"\\") {
        return Some(
            "a network path cannot be used: the service runs as SYSTEM, which has no network identity"
                .into(),
        );
    }
    if needs_system {
        let user_ish = [r"\Users\", r"\Documents", r"\Desktop"];
        let s = dir.to_string_lossy().to_ascii_lowercase();
        if user_ish.iter().any(|p| s.contains(&p.to_ascii_lowercase())) {
            return Some(format!(
                "{} is inside a user profile; the service runs as SYSTEM and may not be able to write there",
                dir.display()
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::drives::{DriveInfo, Journal};
    use super::super::prompt::Scripted;
    use super::*;

    fn drives() -> Vec<DriveInfo> {
        vec![
            DriveInfo {
                letter: 'C',
                label: "OS".into(),
                filesystem: "NTFS".into(),
                total: 585_000_000_000,
                free: 157_000_000_000,
                journal: Journal::Active,
            },
            DriveInfo {
                letter: 'D',
                label: "Data".into(),
                filesystem: "NTFS".into(),
                total: 1_292_000_000_000,
                free: 674_000_000_000,
                journal: Journal::Absent,
            },
            DriveInfo {
                letter: 'E',
                label: "Stick".into(),
                filesystem: "exFAT".into(),
                total: 32_000_000_000,
                free: 8_000_000_000,
                journal: Journal::Unknown,
            },
        ]
    }

    /// Answers for the shortest path: service, all drives, all defaults.
    fn all_defaults() -> Vec<&'static str> {
        vec![
            "", // 1  service? default yes
            "", // 2  drives, default = all NTFS
            "", // 3  exclude system, default yes
            "", // 4  data dir, default
            "", // 4b restrict, default yes
            "", // 4c keep in memory, default yes
            "", // 5  hotkey? default yes
            "", // 5  which, default Ctrl+Space
            "", // 6  PATH, default yes
        ]
    }

    fn run_with(answers: Vec<&str>) -> (SetupPlan, Scripted) {
        let mut s = Scripted::new(answers);
        let plan = run(&mut s, &drives(), &Config::default()).expect("flow should complete");
        (plan, s)
    }

    #[test]
    fn accepting_every_default_produces_a_sensible_plan() {
        let (plan, s) = run_with(all_defaults());
        assert_eq!(plan.resident, Resident::Service);
        assert_eq!(
            plan.drives,
            vec!['C', 'D'],
            "the exFAT stick must not be included"
        );
        assert!(plan.exclude_system);
        assert!(plan.restrict_index);
        assert_eq!(plan.hotkey.as_deref(), Some("Ctrl+Space"));
        assert!(plan.add_to_path);
        assert_eq!(plan.data_dir, Config::default().data_dir);
        assert_eq!(s.remaining(), 0, "every scripted answer should be consumed");
    }

    #[test]
    fn choosing_the_service_skips_the_scheduled_task_questions() {
        // 1 -> yes must not ask 1b or 1b-a at all.
        let (plan, s) = run_with(all_defaults());
        assert_eq!(plan.resident, Resident::Service);
        assert!(
            !s.mentioned("scheduled task"),
            "1b must be skipped entirely"
        );
        assert!(!s.mentioned("How often"), "1b-a must be skipped entirely");
    }

    #[test]
    fn declining_the_service_offers_the_task_and_asks_the_interval() {
        let (plan, s) = run_with(vec!["n", "y", "30", "", "", "", "", "", "", ""]);
        assert_eq!(plan.resident, Resident::Task { minutes: 30 });
        assert!(s.mentioned("scheduled task"));
        assert!(s.mentioned("How often"));
    }

    #[test]
    fn declining_both_still_reaches_the_remaining_steps() {
        let (plan, s) = run_with(vec!["n", "n", "", "", "", "", "", "", ""]);
        assert_eq!(plan.resident, Resident::Manual);
        assert!(
            !s.mentioned("How often"),
            "no interval when there is no task"
        );
        // Steps 2 to 6 still ran.
        assert_eq!(plan.drives, vec!['C', 'D']);
        assert!(plan.add_to_path);
    }

    #[test]
    fn a_short_interval_is_accepted_but_warned_about() {
        let (plan, s) = run_with(vec!["n", "y", "1", "", "", "", "", "", "", ""]);
        assert_eq!(plan.resident, Resident::Task { minutes: 1 });
        assert!(
            s.mentioned("rewrites the whole index"),
            "should warn at 1 minute"
        );

        let (_, s) = run_with(vec!["n", "y", "15", "", "", "", "", "", "", ""]);
        assert!(
            !s.mentioned("rewrites the whole index"),
            "15 minutes needs no warning"
        );
    }

    #[test]
    fn an_invalid_interval_is_reprompted_not_accepted() {
        let (plan, _) = run_with(vec![
            "n", "y", "0", "999", "banana", "45", "", "", "", "", "", "", "",
        ]);
        assert_eq!(plan.resident, Resident::Task { minutes: 45 });
    }

    #[test]
    fn drives_can_be_chosen_individually_or_all() {
        let mut a = all_defaults();
        a[1] = "1";
        let (plan, _) = run_with(a);
        assert_eq!(plan.drives, vec!['C']);

        let mut a = all_defaults();
        a[1] = "all";
        let (plan, _) = run_with(a);
        assert_eq!(plan.drives, vec!['C', 'D']);
    }

    #[test]
    fn the_hotkey_conflict_is_stated_before_the_choice() {
        let (_, s) = run_with(all_defaults());
        assert!(
            s.mentioned("VS Code"),
            "the wizard must name what loses the shortcut"
        );
        for k in HOTKEYS {
            assert!(s.mentioned(k), "alternative {k} should be offered");
        }
    }

    #[test]
    fn an_alternative_hotkey_can_be_chosen_or_declined() {
        let mut a = all_defaults();
        a[7] = "2";
        let (plan, _) = run_with(a);
        assert_eq!(plan.hotkey.as_deref(), Some("Ctrl+Alt+Space"));

        // Declining skips the follow-up question.
        let (plan, _) = run_with(vec!["", "", "", "", "", "", "n", ""]);
        assert_eq!(plan.hotkey, None);
    }

    #[test]
    fn declining_the_lockdown_says_what_it_costs() {
        let mut a = all_defaults();
        a[4] = "n";
        let (plan, s) = run_with(a);
        assert!(!plan.restrict_index);
        assert!(
            s.mentioned("any local account"),
            "the consequence must be stated"
        );
    }

    #[test]
    fn a_custom_data_directory_is_taken() {
        let mut a = all_defaults();
        a[3] = r"E:\BatutaIndex";
        let (plan, _) = run_with(a);
        assert_eq!(plan.data_dir, PathBuf::from(r"E:\BatutaIndex"));
    }

    #[test]
    fn the_plan_becomes_the_config_that_gets_written() {
        let (plan, _) = run_with(all_defaults());
        let cfg = plan.to_config(&Config::default());
        assert_eq!(cfg.drives, vec!['C', 'D']);
        assert_eq!(cfg.exclude.len(), 3);
        assert!(cfg.restrict_index);
        assert_eq!(cfg.hotkey.as_deref(), Some("Ctrl+Space"));
        assert_eq!(
            cfg.snapshot_path(),
            Config::default().data_dir.join("index.bin")
        );
    }

    #[test]
    fn declining_exclusions_clears_them() {
        let mut a = all_defaults();
        a[2] = "n";
        let (plan, _) = run_with(a);
        let cfg = plan.to_config(&Config::default());
        assert!(cfg.exclude.is_empty());
    }

    #[test]
    fn the_summary_describes_every_branch_it_will_take() {
        let (plan, _) = run_with(all_defaults());
        let s = plan.summary();
        for expect in [
            "index C:, D:",
            "Batuta service",
            "Ctrl+Space",
            "PATH",
            "restrict",
        ] {
            assert!(
                s.to_lowercase().contains(&expect.to_lowercase()),
                "summary missing {expect:?}:\n{s}"
            );
        }

        let (plan, _) = run_with(vec!["n", "y", "20", "", "", "", "", "", "", ""]);
        let s = plan.summary();
        assert!(s.contains("every 20 minute"), "{s}");
        assert!(
            !s.contains("service"),
            "a task plan must not claim to install a service"
        );

        let (plan, _) = run_with(vec!["n", "n", "", "", "", "", "n", "n"]);
        let s = plan.summary();
        assert!(
            s.contains("batuta scan"),
            "manual plans should say how to refresh"
        );
        assert!(!s.contains("PATH"));
    }

    #[test]
    fn a_bad_data_directory_is_caught_before_anything_is_built() {
        assert!(check_data_dir(std::path::Path::new("relative/path"), false).is_some());
        assert!(
            check_data_dir(std::path::Path::new(r"\\server\share\idx"), false)
                .unwrap()
                .contains("network")
        );

        // A user-profile path is fine without a service, but not with one.
        let user = std::path::Path::new(r"C:\Users\Hacker\BatutaIndex");
        assert!(check_data_dir(user, false).is_none());
        assert!(check_data_dir(user, true).unwrap().contains("SYSTEM"));

        assert!(check_data_dir(std::path::Path::new(r"C:\ProgramData\Batuta"), true).is_none());
    }
}
