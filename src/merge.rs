//! `justless run --merge`, which a hook writes for a command that ended in `2>&1`.
//!
//! The shell's `2>&1` joins the stdout and stderr of `justless run` itself, but the
//! command would still write two pipes, and Justless would print the stdout preview
//! before the stderr preview. With `--merge`, the command writes both streams to one
//! pipe, as the original `2>&1` gave it, and Justless archives that pipe as stdout
//! within `max_run_bytes`. The archive, its replay and a verbatim preview keep the
//! order in which the lines were written, and the stderr archive stays empty.
use crate::filters::Profile;

/// Whether a `--merge` run gives the command one pipe for both streams. Search and
/// listing groups read stdout alone and would count the messages of a joined
/// stderr, such as an unreadable directory, as unparsed lines, so these runs keep
/// the streams apart, and the footer says that stderr follows stdout.
pub fn joins(merge: bool, profile: Option<Profile>) -> bool {
    merge && !matches!(profile, Some(Profile::Search | Profile::Listing))
}
