//! Independent runs inside one test, side by side: each case of a test that
//! loops over binaries and cases builds a world of its own (own label, own
//! socket dir) and mostly waits (on timers, terminals, pane programs), so
//! running them at once takes about as long as the slowest instead of their
//! sum.

use std::sync::Mutex;
use std::thread;

/// `f` over every item on `limit` threads at most, the results in item
/// order. A run that panics is re-raised here once every thread has ended,
/// so the other runs' worlds still tear down (their `Drop`s kill their
/// servers); the panic message is printed when it happens, as in any test.
pub fn map<T: Sync, R: Send>(items: &[T], limit: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = Mutex::new(0..items.len());
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    let panics = thread::scope(|s| {
        let workers: Vec<_> = (0..limit.clamp(1, items.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    loop {
                        // The guard goes before the run: `while let` would
                        // hold it for the body and run one at a time.
                        let i = next.lock().unwrap().next();
                        let Some(i) = i else { break };
                        let r = f(&items[i]);
                        results.lock().unwrap()[i] = Some(r);
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .filter_map(|w| w.join().err())
            .collect::<Vec<_>>()
    });
    if let Some(panic) = panics.into_iter().next() {
        std::panic::resume_unwind(panic);
    }
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("every item ran"))
        .collect()
}

/// `f` over every item, each on a thread of its own.
#[allow(dead_code, reason = "not every test binary that includes this uses it")]
pub fn each<T: Sync>(items: &[T], f: impl Fn(&T) + Sync) {
    map(items, items.len(), f);
}
