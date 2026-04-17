//! Process-tree helpers shared between the daemon (for killing a job's
//! full descendant set on shutdown / kill / watchdog) and handoff (for
//! force-killing a sub-agent that emitted its terminal result but whose
//! Bash-tool child shells are holding its stdout pipe open).

use std::collections::HashSet;

/// Snapshots the OS process table as `(pid, ppid, pgid)` tuples. Uses
/// `ps -A -o pid=,ppid=,pgid=` which is portable across macOS and Linux.
/// Returns an empty vec on any `ps` failure.
pub fn snapshot_process_table() -> Vec<(u32, u32, u32)> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .output();
    let Ok(output) = output else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let ppid: u32 = parts.next()?.parse().ok()?;
            let pgid: u32 = parts.next()?.parse().ok()?;
            Some((pid, ppid, pgid))
        })
        .collect()
}

/// BFS expansion of a root PID's descendant set using a pre-sampled
/// process table. Extracted for deterministic unit testing. Pure: no I/O,
/// no process side effects.
pub fn expand_descendants(table: &[(u32, u32, u32)], root_pid: u32) -> HashSet<u32> {
    let mut descendants: HashSet<u32> = HashSet::new();
    descendants.insert(root_pid);
    let mut changed = true;
    while changed {
        changed = false;
        for (pid, ppid, _pgid) in table {
            if !descendants.contains(pid) && descendants.contains(ppid) {
                descendants.insert(*pid);
                changed = true;
            }
        }
    }
    descendants
}

/// Projects a descendant PID set to the unique set of PGIDs those
/// descendants belong to. Skips pgid 0/1 so we never signal init or the
/// daemon's own pgroup.
pub fn collect_pgids_from_descendants(
    table: &[(u32, u32, u32)],
    descendants: &HashSet<u32>,
) -> Vec<u32> {
    let mut pgids: HashSet<u32> = HashSet::new();
    for (pid, _ppid, pgid) in table {
        if descendants.contains(pid) && *pgid > 1 {
            pgids.insert(*pgid);
        }
    }
    pgids.into_iter().collect()
}

/// Walks the live process table rooted at `root_pid` and returns every
/// distinct PGID under that root (including root's own). Used to reach
/// grand-children in pgroups that don't match the one the daemon
/// tracked — claude CLI's Bash tool calls `setpgid` on each spawned
/// command, so a sub-agent's shell scripts end up as pgroup leaders of
/// their own groups and are invisible to `kill -<tracked_pgid>`.
pub fn collect_descendant_pgids(root_pid: u32) -> Vec<u32> {
    let table = snapshot_process_table();
    let descendants = expand_descendants(&table, root_pid);
    collect_pgids_from_descendants(&table, &descendants)
}

/// SIGKILLs every pgroup in the live descendant tree of `root_pid`.
/// No-op on non-unix targets. Safe to call on a dead root — collect
/// returns an empty list and signal syscalls become no-ops.
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn kill_descendant_pgids(root_pid: u32) {
    if root_pid == 0 {
        return;
    }
    for pgid in collect_descendant_pgids(root_pid) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orphan_like_table() -> Vec<(u32, u32, u32)> {
        vec![
            (1,   0,   1),
            (10,  1,   10),
            (100, 10,  100),
            (110, 100, 100),
            (200, 100, 200),
            (201, 200, 200),
            (300, 100, 300),
            (301, 300, 300),
            (500, 1,   500),
        ]
    }

    #[test]
    fn expand_descendants_walks_full_subtree() {
        let table = orphan_like_table();
        let d = expand_descendants(&table, 100);
        assert!(d.contains(&100));
        assert!(d.contains(&110));
        assert!(d.contains(&200));
        assert!(d.contains(&201));
        assert!(d.contains(&300));
        assert!(d.contains(&301));
        assert!(!d.contains(&10));
        assert!(!d.contains(&500));
    }

    #[test]
    fn collect_pgids_covers_all_descendant_pgroups() {
        let table = orphan_like_table();
        let d = expand_descendants(&table, 100);
        let pgids: HashSet<u32> = collect_pgids_from_descendants(&table, &d)
            .into_iter()
            .collect();
        assert!(pgids.contains(&100));
        assert!(pgids.contains(&200));
        assert!(pgids.contains(&300));
        assert_eq!(pgids.len(), 3);
    }

    #[test]
    fn collect_pgids_skips_init_and_zero() {
        let table = vec![
            (1, 0, 1),
            (5, 1, 0),
            (7, 5, 7),
        ];
        let d = expand_descendants(&table, 5);
        let pgids: Vec<u32> = collect_pgids_from_descendants(&table, &d);
        assert_eq!(pgids, vec![7]);
    }

    #[test]
    fn expand_descendants_handles_missing_root_gracefully() {
        let table = orphan_like_table();
        let d = expand_descendants(&table, 9999);
        assert_eq!(d.len(), 1);
        assert!(d.contains(&9999));
    }
}
