use std::collections::{BTreeSet, VecDeque};
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessStat {
    pub pid: u32,
    pub process_group_id: i32,
    pub tty_nr: i32,
    pub foreground_process_group_id: i32,
}

pub(crate) fn foreground_process_argvs_for_pane_shell(pane_pid: Option<u32>) -> Vec<Vec<String>> {
    let Some(pane_pid) = pane_pid else {
        return Vec::new();
    };
    let Ok(shell_stat) = read_process_stat(pane_pid) else {
        return Vec::new();
    };
    let descendants = descendant_process_stats(pane_pid);
    let mut pids = foreground_process_ids_for_shell(&shell_stat, &descendants);
    if shell_stat.process_group_id == shell_stat.foreground_process_group_id {
        pids.push(pane_pid);
    }
    pids.into_iter()
        .filter_map(|pid| process_argv(pid).ok())
        .filter(|argv| !argv.is_empty())
        .collect()
}

pub(crate) fn foreground_process_ids_for_shell(
    shell_stat: &ProcessStat,
    descendants: &[ProcessStat],
) -> Vec<u32> {
    if shell_stat.foreground_process_group_id <= 0 {
        return Vec::new();
    }

    let mut matches = descendants
        .iter()
        .filter(|stat| {
            stat.tty_nr == shell_stat.tty_nr
                && stat.process_group_id == shell_stat.foreground_process_group_id
        })
        .map(|stat| stat.pid)
        .collect::<Vec<_>>();
    matches.sort_unstable();

    let group_leader = shell_stat.foreground_process_group_id as u32;
    let mut ordered = Vec::with_capacity(matches.len());
    if matches.contains(&group_leader) {
        ordered.push(group_leader);
    }
    ordered.extend(matches.into_iter().filter(|pid| *pid != group_leader));
    ordered
}

fn process_argv(pid: u32) -> std::io::Result<Vec<String>> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(cmdline
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect())
}

fn descendant_process_stats(root_pid: u32) -> Vec<ProcessStat> {
    let mut visited = BTreeSet::new();
    let mut pending = VecDeque::from(read_process_children(root_pid).unwrap_or_default());
    let mut descendants = Vec::new();

    while let Some(pid) = pending.pop_front() {
        if !visited.insert(pid) {
            continue;
        }
        if let Ok(stat) = read_process_stat(pid) {
            pending.extend(read_process_children(pid).unwrap_or_default());
            descendants.push(stat);
        }
    }

    descendants
}

fn read_process_children(pid: u32) -> std::io::Result<Vec<u32>> {
    let children = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))?;
    Ok(parse_process_children(&children))
}

pub(crate) fn parse_process_children(children: &str) -> Vec<u32> {
    children
        .split_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
        .collect()
}

fn read_process_stat(pid: u32) -> std::io::Result<ProcessStat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_process_stat(&stat).map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))
}

pub(crate) fn parse_process_stat(stat: &str) -> Result<ProcessStat, String> {
    let stat = stat.trim();
    let command_end = stat
        .rfind(')')
        .ok_or_else(|| format!("process stat is missing command terminator: `{stat}`"))?;
    let fields = stat[command_end + 2..]
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() < 6 {
        return Err(format!("process stat has too few fields: `{stat}`"));
    }

    Ok(ProcessStat {
        pid: parse_process_stat_field(stat, 0, "pid")?,
        process_group_id: parse_process_stat_field(fields[2], 0, "process group id")?,
        tty_nr: parse_process_stat_field(fields[4], 0, "tty nr")?,
        foreground_process_group_id: parse_process_stat_field(
            fields[5],
            0,
            "foreground process group id",
        )?,
    })
}

fn parse_process_stat_field<T>(source: &str, index: usize, field_name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let value = source
        .split_whitespace()
        .nth(index)
        .ok_or_else(|| format!("process stat is missing {field_name}"))?;
    value
        .parse::<T>()
        .map_err(|_| format!("failed to parse {field_name} from `{value}`"))
}
