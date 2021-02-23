#[macro_use]
extern crate lazy_static;

#[macro_use]
pub mod utils;
pub mod exec;
pub mod fuzz;
pub mod gen;
pub mod model;
pub mod targets;

use crate::exec::{ExecConf, QemuConf, SshConf};
use crate::fuzz::{
    features,
    fuzzer::{Fuzzer, Mode},
    queue::Queue,
    relation::Relation,
    stats::{bench, Stats},
};
use crate::targets::Target;

use std::{
    collections::VecDeque,
    fs::{create_dir, read_to_string},
    io::ErrorKind,
    path::PathBuf,
    process::exit,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Debug, Clone)]
pub struct Config {
    pub target: String,
    pub kernel_obj_dir: Option<PathBuf>,
    pub kernel_src_dir: Option<PathBuf>,
    pub syz_bin_dir: PathBuf,
    pub out_dir: PathBuf,
    pub relations: Option<PathBuf>,
    pub jobs: u64,
    pub skip_repro: bool,
    pub disabled_calls: Option<PathBuf>,

    pub qemu_conf: QemuConf,
    pub exec_conf: ExecConf,
    pub ssh_conf: SshConf,
}

impl Config {
    pub fn check(&self) -> Result<(), String> {
        let supported = targets::sys_json::supported();
        if !supported.contains(&self.target) {
            return Err(format!(
                "unspported target: {}.({:?} are supported)",
                self.target, supported
            ));
        }
        if let Some(dir) = self.kernel_obj_dir.as_ref() {
            if !dir.is_dir() {
                return Err(format!(
                    "bad kernel object file directory '{}'.",
                    dir.display()
                ));
            }
        }
        if let Some(dir) = self.kernel_src_dir.as_ref() {
            if !dir.is_dir() {
                return Err(format!(
                    "bad kernel srouce files directory '{}'.",
                    dir.display()
                ));
            }
        }
        if let Some(f) = self.disabled_calls.as_ref() {
            if !f.is_file() {
                return Err(format!("bad disabled system calls file: {}", f.display()));
            }
        }
        if !self.syz_bin_dir.is_dir() {
            return Err(format!(
                "bad syzkaller binary files directory '{}'.",
                self.syz_bin_dir.display()
            ));
        }

        let target_bins = vec!["syz-executor", "syz-execprog", "syz-fuzzer"];
        let dir = self.syz_bin_dir.join(self.target.replace("/", "_"));
        for bin in &target_bins {
            let f = dir.join(bin);
            if !f.is_file() {
                return Err(format!(
                    "missing executable file {} in {}",
                    bin,
                    dir.display()
                ));
            }
        }
        let symbolize = self.syz_bin_dir.join("syz-symbolize");
        if !symbolize.is_file() {
            return Err(format!(
                "missing executable file syz-symbolize in {}",
                self.syz_bin_dir.display()
            ));
        }
        self.qemu_conf
            .check()
            .map_err(|e| format!("qemu config: {}", e))?;
        self.ssh_conf
            .check()
            .map_err(|e| format!("ssh config: {}", e))
    }
}

pub fn start(conf: Config) {
    if let Err(e) = conf.check() {
        log::error!("config error: {}", e);
        exit(1);
    }

    let max_cov = Arc::new(RwLock::new(FxHashSet::default()));
    let calibrated_cov = Arc::new(RwLock::new(FxHashSet::default()));
    let crashes = Arc::new(Mutex::new(FxHashMap::default()));
    let repros = Arc::new(Mutex::new(FxHashMap::default()));
    let raw_crashes = Arc::new(Mutex::new(VecDeque::with_capacity(1024)));
    let stats = Arc::new(Stats::new());
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(conf.jobs as usize + 1));
    let mut fuzzers = Vec::new();

    if let Err(e) = create_dir(&conf.out_dir) {
        if e.kind() == ErrorKind::AlreadyExists {
            let crash_dir = conf.out_dir.join("crashes");
            if crash_dir.exists() {
                log::warn!(
                    "Existing crash data ({}) may be overwritten",
                    crash_dir.display()
                );
            }
        } else {
            log::error!(
                "Failed to create output directory {}: {}",
                conf.out_dir.display(),
                e
            );
            exit(1);
        }
    }

    println!("{}", HEALER);

    log::info!("Loading target {}...", conf.target);
    let mut disabled_calls = FxHashSet::default();
    if let Some(f) = conf.disabled_calls.as_ref() {
        let calls = read_to_string(f).unwrap_or_else(|e| {
            log::error!(
                "failed to read disabled system calls file '{}': {}",
                f.display(),
                e
            );
            exit(1)
        });
        disabled_calls = calls
            .lines()
            .filter(|&l| !l.is_empty())
            .map(|c| c.trim().to_string())
            .collect();
    }
    let target = Target::new(&conf.target, &disabled_calls).unwrap_or_else(|e| {
        // preloading.
        log::error!("failed to load target '{}': {}", conf.target, e);
        exit(1)
    });
    log::info!("Revision: {}", &target.revision[0..12]);
    log::info!(
        "Syscalls: {}/{}",
        target.syscalls.len(),
        target.all_syscalls.len()
    );

    let relations_file = if let Some(f) = conf.relations.as_ref() {
        f.clone()
    } else {
        conf.out_dir.join("relations")
    };
    let relations = Relation::load(&target, &relations_file).unwrap_or_else(|e| {
        log::error!(
            "Failed to load relations '{}': {}",
            relations_file.display(),
            e
        );
        exit(1);
    });
    let relations = Arc::new(relations);
    log::info!("Initial relations: {}", relations.len());

    log::info!("Boot {} {} on qemu...", conf.jobs, conf.target);
    let start = Instant::now();
    for id in 0..conf.jobs {
        let max_cov = Arc::clone(&max_cov);
        let calibrated_cov = Arc::clone(&calibrated_cov);
        let relations = Arc::clone(&relations);
        let crashes = Arc::clone(&crashes);
        let repros = Arc::clone(&repros);
        let raw_crashes = Arc::clone(&raw_crashes);
        let stats = Arc::clone(&stats);
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        let conf = conf.clone();
        let disabled_calls = disabled_calls.clone();

        let handle = thread::spawn(move || {
            let conf = conf.clone();
            let target = Target::new(&conf.target, &disabled_calls).unwrap();
            let mut queue = match Queue::with_outdir(id as usize, conf.out_dir.clone()) {
                Ok(q) => q,
                Err(e) => {
                    log::error!("failed to initialize queue-{}: {}", id, e);
                    exit(1)
                }
            };
            if id == 0 {
                // only record queue-0's stats.
                queue.set_stats(Arc::clone(&stats));
            }

            let mut exec_handle = match exec::spawn_in_qemu(
                conf.exec_conf.clone(),
                conf.qemu_conf.clone(),
                conf.ssh_conf.clone(),
                id,
            ) {
                Ok(handle) => handle,
                Err(e) => {
                    log::error!("failed to boot: {}", e);
                    exit(1)
                }
            };
            let features = features::check(&mut exec_handle, id == 0);
            barrier.wait();

            let mut fuzzer = Fuzzer {
                max_cov,
                calibrated_cov,
                relations,
                crashes,
                repros,
                raw_crashes,
                stats,
                id,
                target,
                conf,
                local_vals: FxHashMap::default(),
                queue,
                exec_handle,
                run_history: VecDeque::with_capacity(128),
                mode: Mode::Sampling,
                mut_gaining: 0,
                gen_gaining: 0,
                features,
                cycle_len: 128,
                last_reboot: Instant::now(),
                stop,
            };
            fuzzer.fuzz();
        });
        fuzzers.push(handle);
    }

    barrier.wait();

    ctrlc::set_handler(move || {
        stop.store(true, Ordering::Relaxed);
        println!("Waiting fuzzers to exit...");
        while !fuzzers.is_empty() {
            let f = fuzzers.pop().unwrap();
            f.join().unwrap();
        }
        println!("Ok, have a nice day. *-*");
        exit(0)
    })
    .unwrap();

    log::info!("Boot finished, cost {}s", start.elapsed().as_secs());
    log::info!("Let the fuzz begin");
    bench(Duration::new(10, 0), conf.out_dir, stats);
}

const HEALER: &str = r"
 ___   ___   ______   ________   __       ______   ______
/__/\ /__/\ /_____/\ /_______/\ /_/\     /_____/\ /_____/\
\::\ \\  \ \\::::_\/_\::: _  \ \\:\ \    \::::_\/_\:::_ \ \
 \::\/_\ .\ \\:\/___/\\::(_)  \ \\:\ \    \:\/___/\\:(_) ) )_
  \:: ___::\ \\::___\/_\:: __  \ \\:\ \____\::___\/_\: __ `\ \
   \: \ \\::\ \\:\____/\\:.\ \  \ \\:\/___/\\:\____/\\ \ `\ \ \
    \__\/ \::\/ \_____\/ \__\/\__\/ \_____\/ \_____\/ \_\/ \_\/
";
