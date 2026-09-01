//! The scheduled-task branch (step 1b).
//!
//! Command construction is a pure function so the quoting can be tested.
//! Install paths contain spaces, and that is exactly where this breaks.

use std::path::Path;

/// Name of the task Quick Setup creates.
pub const TASK_NAME: &str = "Batuta Index";

/// Arguments for `schtasks /create`.
///
/// `/tr` takes a single string that Task Scheduler re-parses as a command
/// line, so the executable path inside it needs its own quotes: without them a
/// path like `C:\Program Files\Batuta\batuta.exe` would be read as the program
/// `C:\Program` with `Files\Batuta\batuta.exe` as an argument.
///
/// `/ru SYSTEM` avoids both storing a password and prompting for elevation on
/// every run.
pub fn create_args(exe: &Path, minutes: u32) -> Vec<String> {
    vec![
        "/create".into(),
        "/tn".into(),
        TASK_NAME.into(),
        "/tr".into(),
        format!("\"{}\" scan", exe.display()),
        "/sc".into(),
        "minute".into(),
        "/mo".into(),
        minutes.to_string(),
        "/ru".into(),
        "SYSTEM".into(),
        "/rl".into(),
        "HIGHEST".into(),
        "/f".into(),
    ]
}

/// Arguments for removing the task.
pub fn delete_args() -> Vec<String> {
    vec![
        "/delete".into(),
        "/tn".into(),
        TASK_NAME.into(),
        "/f".into(),
    ]
}

/// Arguments for checking the task exists.
pub fn query_args() -> Vec<String> {
    vec!["/query".into(), "/tn".into(), TASK_NAME.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_after(args: &[String], flag: &str) -> String {
        let i = args.iter().position(|a| a == flag).expect("flag missing");
        args[i + 1].clone()
    }

    #[test]
    fn the_executable_is_quoted_inside_the_task_action() {
        // The case that breaks: a path with a space.
        let args = create_args(Path::new(r"C:\Program Files\Batuta\batuta.exe"), 15);
        let tr = arg_after(&args, "/tr");
        assert_eq!(tr, r#""C:\Program Files\Batuta\batuta.exe" scan"#);
        assert!(tr.starts_with('"'), "the program path must be quoted");
        assert!(
            tr.ends_with(" scan"),
            "the subcommand stays outside the quotes"
        );
    }

    #[test]
    fn the_interval_reaches_the_command() {
        for m in [1u32, 15, 60] {
            let args = create_args(Path::new(r"C:\Batuta\batuta.exe"), m);
            assert_eq!(arg_after(&args, "/mo"), m.to_string());
            assert_eq!(arg_after(&args, "/sc"), "minute");
        }
    }

    #[test]
    fn the_task_runs_as_system_at_highest_privilege() {
        // Anything less would prompt for elevation on every run, or fail to
        // open a raw volume handle when it did run.
        let args = create_args(Path::new(r"C:\Batuta\batuta.exe"), 15);
        assert_eq!(arg_after(&args, "/ru"), "SYSTEM");
        assert_eq!(arg_after(&args, "/rl"), "HIGHEST");
        assert!(
            args.iter().any(|a| a == "/f"),
            "must overwrite an existing task"
        );
    }

    #[test]
    fn the_task_name_is_consistent_across_operations() {
        let create = arg_after(&create_args(Path::new("x.exe"), 5), "/tn");
        assert_eq!(create, TASK_NAME);
        assert_eq!(arg_after(&delete_args(), "/tn"), TASK_NAME);
        assert_eq!(arg_after(&query_args(), "/tn"), TASK_NAME);
    }

    #[test]
    fn arguments_are_passed_separately_not_concatenated() {
        // Each element is one argv entry, so the OS does no re-quoting of the
        // task name itself even though it contains a space.
        let args = create_args(Path::new("x.exe"), 5);
        assert!(args.contains(&"Batuta Index".to_string()));
        assert!(!args.iter().any(|a| a.contains("/tn Batuta")));
    }
}
