//! Starting work before its inputs are final, on channels nothing else is using.
//!
//! A speculative task runs against the best draft of its input so far. Its result is kept
//! only if the input turns out to be unchanged; otherwise it is dropped. The scheduler here
//! decides what to start and when. It knows nothing about models.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct Task<'a> {
    pub name: &'static str,
    /// Higher starts first. See [`priority`].
    pub priority: f64,
    /// Another task whose successful result this one needs; it does not start before then.
    pub requires: Option<&'static str>,
    pub run: Box<dyn FnOnce() -> Result<String, String> + Send + 'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub name: &'static str,
    pub result: Result<String, String>,
}

/// How urgently to spend spare capacity on a task: the more of the work depends on it
/// (`dependents`) and the less is known about how it will turn out (`unknown`, 0 to 1), the
/// sooner it should run, because it is the work whose outcome is most likely to change the
/// plan. A task nothing depends on and whose result is predictable comes last.
pub fn priority(dependents: usize, unknown: f64) -> f64 {
    (1.0 + dependents as f64) * (0.25 + unknown.clamp(0.0, 1.0))
}

/// A stable hash of a text, for telling whether a draft became the final version.
pub fn fingerprint(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Wait for a draft that has stopped changing: `read` returns the current text (or `None`
/// while there is none), and the draft counts as settled once it has been the same for
/// `stable`. Returns `None` if `finished` is set first, meaning the final version arrived
/// before any draft could be trusted.
pub fn wait_for_draft(
    read: &dyn Fn() -> Option<String>,
    finished: &AtomicBool,
    stable: Duration,
    poll: Duration,
) -> Option<String> {
    let mut last: Option<(String, Instant)> = None;
    loop {
        if finished.load(Ordering::Relaxed) {
            return None;
        }
        match read().filter(|t| !t.trim().is_empty()) {
            Some(text) => match &last {
                Some((prev, since)) if *prev == text => {
                    if since.elapsed() >= stable {
                        return Some(text);
                    }
                }
                _ => last = Some((text, Instant::now())),
            },
            None => last = None,
        }
        std::thread::sleep(poll);
    }
}

/// Run `tasks`, highest priority first. A task starts only while `free()` reports more than
/// `reserve` unused channels, so the main path always has room. One task starts per `poll`,
/// which gives the count of free channels time to reflect the one just started. Once `stop`
/// is set nothing new starts; tasks already running are left to finish (they watch their own
/// cancel flag). Returns the finished tasks; tasks that never started are not in the result.
pub fn run(
    tasks: Vec<Task<'_>>,
    free: &(dyn Fn() -> usize + Sync),
    reserve: usize,
    stop: &AtomicBool,
    poll: Duration,
) -> Vec<Done> {
    let mut queue = tasks;
    queue.sort_by(|a, b| b.priority.total_cmp(&a.priority));
    std::thread::scope(|scope| {
        let mut running = Vec::new();
        let mut done: Vec<Done> = Vec::new();
        loop {
            let mut i = 0;
            while i < running.len() {
                let (_, handle): &(&'static str, std::thread::ScopedJoinHandle<'_, _>) = &running[i];
                if handle.is_finished() {
                    let (name, handle) = running.remove(i);
                    let result = handle
                        .join()
                        .unwrap_or_else(|_| Err("the task panicked".to_string()));
                    done.push(Done { name, result });
                } else {
                    i += 1;
                }
            }

            let failed: Vec<&'static str> = done
                .iter()
                .filter(|d| d.result.is_err())
                .map(|d| d.name)
                .collect();
            let mut k = 0;
            while k < queue.len() {
                match queue[k].requires {
                    Some(r) if failed.contains(&r) => {
                        let t = queue.remove(k);
                        done.push(Done {
                            name: t.name,
                            result: Err(format!("not started: {r} did not finish")),
                        });
                    }
                    _ => k += 1,
                }
            }

            if !stop.load(Ordering::Relaxed) && free() > reserve {
                let ready = queue.iter().position(|t| {
                    t.requires.is_none_or(|r| done.iter().any(|d| d.name == r && d.result.is_ok()))
                });
                if let Some(ix) = ready {
                    let t = queue.remove(ix);
                    let run_it = t.run;
                    running.push((t.name, scope.spawn(run_it)));
                }
            }

            let nothing_more = queue.is_empty() || stop.load(Ordering::Relaxed);
            if running.is_empty() && nothing_more {
                break;
            }
            std::thread::sleep(poll);
        }
        done
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    const POLL: Duration = Duration::from_millis(5);

    fn task<'a>(
        name: &'static str,
        priority: f64,
        requires: Option<&'static str>,
        log: &'a Mutex<Vec<String>>,
        active: &'a AtomicUsize,
        peak: &'a AtomicUsize,
        millis: u64,
    ) -> Task<'a> {
        Task {
            name,
            priority,
            requires,
            run: Box::new(move || {
                log.lock().unwrap().push(format!("start {name}"));
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(millis));
                active.fetch_sub(1, Ordering::SeqCst);
                log.lock().unwrap().push(format!("end {name}"));
                Ok(format!("{name} output"))
            }),
        }
    }

    #[test]
    fn central_and_unknown_work_starts_before_peripheral_and_predictable_work() {
        assert!(priority(5, 0.5) > priority(1, 0.5), "more dependents first");
        assert!(priority(1, 0.9) > priority(1, 0.1), "more unknown first");
        assert!(priority(5, 0.1) > priority(1, 0.2), "central beats slightly more unknown");
        assert!(priority(0, 0.0) > 0.0);
    }

    #[test]
    fn tasks_start_in_priority_order() {
        let (log, active, peak) = (Mutex::new(vec![]), AtomicUsize::new(0), AtomicUsize::new(0));
        let tasks = vec![
            task("low", 1.0, None, &log, &active, &peak, 1),
            task("high", 9.0, None, &log, &active, &peak, 1),
            task("mid", 5.0, None, &log, &active, &peak, 1),
        ];
        let stop = AtomicBool::new(false);
        let done = run(tasks, &|| 10, 0, &stop, POLL);
        assert_eq!(done.len(), 3);
        let starts: Vec<String> = log.lock().unwrap().iter().filter(|l| l.starts_with("start")).cloned().collect();
        assert_eq!(starts, ["start high", "start mid", "start low"]);
    }

    #[test]
    fn the_reserve_is_never_used_and_work_waits_for_capacity() {
        let (log, active, peak) = (Mutex::new(vec![]), AtomicUsize::new(0), AtomicUsize::new(0));
        let tasks = (0..6)
            .map(|i| task(["a", "b", "c", "d", "e", "f"][i], 1.0, None, &log, &active, &peak, 40))
            .collect();
        let stop = AtomicBool::new(false);
        let capacity = 4;
        let reserve = 1;
        let done = run(tasks, &|| capacity - active.load(Ordering::SeqCst), reserve, &stop, POLL);
        assert_eq!(done.len(), 6, "everything runs in the end");
        assert!(peak.load(Ordering::SeqCst) <= capacity - reserve, "peak {}", peak.load(Ordering::SeqCst));
        assert!(peak.load(Ordering::SeqCst) >= 2, "it does use the spare capacity");
    }

    #[test]
    fn a_task_waits_for_what_it_requires() {
        let (log, active, peak) = (Mutex::new(vec![]), AtomicUsize::new(0), AtomicUsize::new(0));
        let tasks = vec![
            task("schema", 9.0, Some("inductive"), &log, &active, &peak, 1),
            task("inductive", 1.0, None, &log, &active, &peak, 20),
        ];
        let stop = AtomicBool::new(false);
        let done = run(tasks, &|| 10, 0, &stop, POLL);
        assert!(done.iter().all(|d| d.result.is_ok()));
        let log = log.lock().unwrap();
        let end_inductive = log.iter().position(|l| l == "end inductive").unwrap();
        let start_schema = log.iter().position(|l| l == "start schema").unwrap();
        assert!(start_schema > end_inductive, "{log:?}");
    }

    #[test]
    fn a_task_whose_requirement_failed_is_not_started() {
        let failing: Task = Task {
            name: "inductive",
            priority: 1.0,
            requires: None,
            run: Box::new(|| Err("boom".to_string())),
        };
        let dependent: Task = Task {
            name: "schema",
            priority: 9.0,
            requires: Some("inductive"),
            run: Box::new(|| panic!("must not run")),
        };
        let stop = AtomicBool::new(false);
        let done = run(vec![dependent, failing], &|| 10, 0, &stop, POLL);
        let schema = done.iter().find(|d| d.name == "schema").unwrap();
        assert_eq!(schema.result, Err("not started: inductive did not finish".to_string()));
    }

    #[test]
    fn stopping_prevents_new_starts_and_lets_running_work_finish() {
        let (log, active, peak) = (Mutex::new(vec![]), AtomicUsize::new(0), AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let tasks = vec![
            task("first", 9.0, None, &log, &active, &peak, 60),
            task("second", 1.0, None, &log, &active, &peak, 1),
        ];
        let done = std::thread::scope(|s| {
            let h = s.spawn(|| run(tasks, &|| if active.load(Ordering::SeqCst) == 0 { 10 } else { 0 }, 0, &stop, POLL));
            std::thread::sleep(Duration::from_millis(25));
            stop.store(true, Ordering::SeqCst);
            h.join().unwrap()
        });
        assert_eq!(done.len(), 1, "only the task already running finished");
        assert_eq!(done[0].name, "first");
        assert!(!log.lock().unwrap().iter().any(|l| l == "start second"));
    }

    #[test]
    fn a_draft_counts_once_it_has_stopped_changing() {
        let finished = AtomicBool::new(false);
        let reads = AtomicUsize::new(0);
        let read = || {
            let n = reads.fetch_add(1, Ordering::SeqCst);
            match n {
                0..=2 => None,
                3..=5 => Some(format!("draft {n}")),
                _ => Some("settled draft".to_string()),
            }
        };
        let got = wait_for_draft(&read, &finished, Duration::from_millis(30), Duration::from_millis(5));
        assert_eq!(got.as_deref(), Some("settled draft"));
    }

    #[test]
    fn no_draft_is_returned_when_the_final_version_comes_first() {
        let finished = AtomicBool::new(false);
        let got = std::thread::scope(|s| {
            let h = s.spawn(|| wait_for_draft(&|| None, &finished, Duration::from_millis(30), Duration::from_millis(5)));
            std::thread::sleep(Duration::from_millis(20));
            finished.store(true, Ordering::SeqCst);
            h.join().unwrap()
        });
        assert_eq!(got, None);
    }

    #[test]
    fn fingerprint_tells_a_changed_draft_from_an_unchanged_one() {
        assert_eq!(fingerprint("the contract"), fingerprint("the contract"));
        assert_ne!(fingerprint("the contract"), fingerprint("the contract."));
    }
}
