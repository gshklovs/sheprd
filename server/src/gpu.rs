//! NVIDIA GPU numbers from `nvidia-smi` (never NVML: `nvmlInit` alone costs
//! ~18 MB RSS). Pure parsers plus a spawner with a 2 s timeout per call.
//! `SHEPRD_FAKE_NVSMI=<dir>` reads `query-gpu.txt`, `compute-apps.txt` and
//! `pmon.txt` from that dir instead (tests, mockup states).

use std::collections::HashMap;
use std::time::Duration;

const MIB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    pub idx: u32,
    pub name: String,
    pub util: Option<f64>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub power_cap: Option<f64>,
    pub fan: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PmonRow {
    pub pid: u32,
    /// `C`, `G` or `C+G`.
    pub kind: String,
    pub sm: Option<f64>,
    pub enc: Option<f64>,
    pub dec: Option<f64>,
    pub fb: u64,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub gpus: Vec<Gpu>,
    /// pid -> VRAM bytes, compute processes only (`--query-compute-apps`).
    pub apps: HashMap<u32, u64>,
    /// Every GPU process (compute and graphics) with SM%; only when asked for.
    pub pmon: Option<Vec<PmonRow>>,
    /// Whether `apps` was queried this time (else it may be carried over).
    pub apps_fresh: bool,
    /// `no-nvidia`, `timeout` or `failed: <stderr first line>`.
    pub err: Option<String>,
}

impl Sample {
    /// VRAM bytes per pid, preferring pmon's framebuffer figure when present.
    pub fn mem_of(&self, pid: u32) -> u64 {
        let a = self.apps.get(&pid).copied().unwrap_or(0);
        let p = self.pmon.as_ref().and_then(|l| l.iter().find(|r| r.pid == pid)).map(|r| r.fb).unwrap_or(0);
        a.max(p)
    }
}

/// `[N/A]`, `N/A`, `-`, `[Not Supported]` → None.
fn num(s: &str) -> Option<f64> {
    let s = s.trim();
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,
/// power.draw,power.limit,fan.speed --format=csv,noheader,nounits`.
pub fn parse_query_gpu(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 5 {
                return None;
            }
            Some(Gpu {
                idx: f[0].parse().ok()?,
                name: f[1].to_string(),
                util: num(f[2]),
                mem_used: (num(f[3]).unwrap_or(0.0) * MIB) as u64,
                mem_total: (num(f[4]).unwrap_or(0.0) * MIB) as u64,
                temp: f.get(5).and_then(|s| num(s)),
                power: f.get(6).and_then(|s| num(s)),
                power_cap: f.get(7).and_then(|s| num(s)),
                fan: f.get(8).and_then(|s| num(s)),
            })
        })
        .collect()
}

/// `--query-compute-apps=pid,used_memory --format=csv,noheader,nounits`.
pub fn parse_compute_apps(text: &str) -> HashMap<u32, u64> {
    let mut m = HashMap::new();
    for l in text.lines() {
        let mut f = l.split(',').map(str::trim);
        let (Some(pid), Some(mem)) = (f.next().and_then(|p| p.parse::<u32>().ok()), f.next()) else { continue };
        *m.entry(pid).or_insert(0) += (num(mem).unwrap_or(0.0) * MIB) as u64;
    }
    m
}

/// `nvidia-smi pmon -c 1 -s um`. Columns are located from the `# gpu pid type …`
/// header, since drivers differ (jpg/ofa/ccpm are newer); the command name is
/// the rest of the line and may contain spaces.
pub fn parse_pmon(text: &str) -> Vec<PmonRow> {
    let mut cols: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        if let Some(h) = t.strip_prefix('#') {
            if cols.is_empty() {
                cols = h.split_whitespace().map(str::to_lowercase).collect();
            }
            continue;
        }
        if t.is_empty() || cols.is_empty() {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let ncmd = cols.iter().position(|c| c == "command").unwrap_or(cols.len());
        if f.len() < ncmd {
            continue;
        }
        let get = |name: &str| cols.iter().position(|c| c == name).and_then(|i| f.get(i).copied());
        let Some(pid) = get("pid").and_then(|p| p.parse::<u32>().ok()) else { continue };
        out.push(PmonRow {
            pid,
            kind: get("type").unwrap_or("").to_string(),
            sm: get("sm").and_then(num),
            enc: get("enc").and_then(num),
            dec: get("dec").and_then(num),
            fb: (get("fb").and_then(num).unwrap_or(0.0) * MIB) as u64,
            name: f[ncmd.min(f.len())..].join(" "),
        });
    }
    // Several rows per pid (one per GPU): sum memory, max SM.
    let mut merged: Vec<PmonRow> = Vec::new();
    for r in out {
        if let Some(m) = merged.iter_mut().find(|m| m.pid == r.pid) {
            m.fb += r.fb;
            m.sm = match (m.sm, r.sm) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        } else {
            merged.push(r);
        }
    }
    merged
}

enum RunErr {
    Missing,
    Timeout,
    Failed(String),
}

async fn run(args: &[&str]) -> Result<String, RunErr> {
    let fut = tokio::process::Command::new("nvidia-smi")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(2), fut).await {
        Err(_) => Err(RunErr::Timeout),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Err(RunErr::Missing),
        Ok(Err(e)) => Err(RunErr::Failed(e.to_string())),
        Ok(Ok(o)) if !o.status.success() => {
            let e = String::from_utf8_lossy(if o.stderr.is_empty() { &o.stdout } else { &o.stderr }).to_string();
            Err(RunErr::Failed(e.lines().next().unwrap_or("exit status").trim().to_string()))
        }
        Ok(Ok(o)) => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
    }
}

fn fake_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SHEPRD_FAKE_NVSMI").filter(|d| !d.is_empty()).map(Into::into)
}

/// One GPU sample. `pmon` adds the per-process SM% call (live tier only; it also
/// carries per-process memory, so `apps` can then be skipped). Measured CPU per
/// call on the GTX 1070 box: query-gpu ~9 ms, compute-apps ~7 ms, pmon ~29 ms.
/// `apps` decides from the adapters' numbers whether to run `--query-compute-apps`
/// (`None` in the result's `apps_fresh` when skipped).
pub async fn sample(pmon: bool, apps: impl FnOnce(&[Gpu]) -> bool) -> Sample {
    if let Some(dir) = fake_dir() {
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
        let Some(q) = read("query-gpu.txt") else {
            return Sample { err: Some("no-nvidia".into()), ..Default::default() };
        };
        let gpus = parse_query_gpu(&q);
        let want = apps(&gpus);
        return Sample {
            gpus,
            apps_fresh: want,
            apps: if want { parse_compute_apps(&read("compute-apps.txt").unwrap_or_default()) } else { HashMap::new() },
            pmon: pmon.then(|| parse_pmon(&read("pmon.txt").unwrap_or_default())),
            err: None,
        };
    }
    let q = run(&[
        "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed",
        "--format=csv,noheader,nounits",
    ])
    .await;
    let q = match q {
        Ok(q) => q,
        Err(e) => {
            let err = match e {
                RunErr::Missing => "no-nvidia".to_string(),
                RunErr::Timeout => "timeout".to_string(),
                RunErr::Failed(s) => format!("failed: {s}"),
            };
            return Sample { err: Some(err), ..Default::default() };
        }
    };
    let gpus = parse_query_gpu(&q);
    let want = apps(&gpus);
    let apps = if want { run(&["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"]).await.ok() } else { None };
    let pm = if pmon { run(&["pmon", "-c", "1", "-s", "um"]).await.ok().map(|t| parse_pmon(&t)) } else { None };
    Sample {
        gpus,
        apps_fresh: apps.is_some(),
        apps: apps.map(|t| parse_compute_apps(&t)).unwrap_or_default(),
        pmon: pm,
        err: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_gpu_with_na() {
        let g = parse_query_gpu("0, NVIDIA GeForce GTX 1070, 97, 6963, 8192, 71, 148.21, 151.00, 62\n");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].util, Some(97.0));
        assert_eq!(g[0].mem_total, 8192 * 1024 * 1024);
        assert_eq!(g[0].power_cap, Some(151.0));
        let g = parse_query_gpu("0, Tesla, [N/A], 10, 100, [N/A], [N/A], [N/A], [Not Supported]");
        assert_eq!(g[0].util, None);
        assert_eq!(g[0].temp, None);
        assert_eq!(g[0].fan, None);
        assert!(parse_query_gpu("").is_empty());
    }

    #[test]
    fn compute_apps() {
        let m = parse_compute_apps("161707, 7\n180809, 78\n\n");
        assert_eq!(m[&180809], 78 * 1024 * 1024);
        assert!(parse_compute_apps("No running processes found").is_empty());
    }

    #[test]
    fn pmon_rows_dash_and_desktop() {
        let t = "# gpu         pid   type     sm    mem    enc    dec    jpg    ofa     fb   ccpm    command \n\
# Idx           #    C/G      %      %      %      %      %      %     MB     MB    name \n\
    0        975     G      -      -      -      -      -      -    181      0    Hyprland       \n\
    0     180809   C+G      0      0      -      -      -      -     78      0    renderD128 --cr\n\
    0      81234     C     97     40      -      -      -      -   6144      0    python3\n";
        let r = parse_pmon(t);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].kind, "G");
        assert_eq!(r[0].sm, None);
        assert_eq!(r[0].fb, 181 * 1024 * 1024);
        assert_eq!(r[1].name, "renderD128 --cr");
        assert_eq!(r[2].sm, Some(97.0));
        // older driver: no jpg/ofa/ccpm columns
        let old = "# gpu pid type sm mem enc dec fb command\n# Idx # C/G % % % % MB name\n 0 42 C 12 3 - - 100 python\n";
        let r = parse_pmon(old);
        assert_eq!(r[0].fb, 100 * 1024 * 1024);
        assert_eq!(r[0].sm, Some(12.0));
        assert!(parse_pmon("").is_empty());
    }

    #[test]
    fn mem_of_prefers_larger() {
        let s = Sample {
            apps: [(7, 10u64)].into_iter().collect(),
            pmon: Some(vec![PmonRow { pid: 7, fb: 20, ..Default::default() }]),
            ..Default::default()
        };
        assert_eq!(s.mem_of(7), 20);
        assert_eq!(s.mem_of(8), 0);
    }
}
