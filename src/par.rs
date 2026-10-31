//! Work spread over a few threads, for reading and decoding files.

use std::sync::atomic::{AtomicUsize, Ordering};

/// `f` over `items` on up to `workers` threads, with the results in the
/// order of `items`. Each thread takes the next item when it finishes one,
/// so one long file holds up only the thread decoding it. An item whose `f`
/// panics is `None`; the rest are unaffected.
pub(crate) fn map<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<Option<R>> {
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers.clamp(1, items.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(item)));
                        done.push((i, r.ok()));
                    }
                    done
                })
            })
            .collect();
        for h in handles {
            for (i, r) in h.join().expect("panics are caught per item") {
                out[i] = r;
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_their_order_and_a_panic_costs_only_its_item() {
        let items: Vec<u32> = (0..50).collect();
        let out = map(&items, 4, |&n| {
            assert!(n != 7, "boom");
            n * 2
        });
        assert_eq!(out.len(), 50);
        assert_eq!(out[7], None);
        for (i, r) in out.iter().enumerate().filter(|(i, _)| *i != 7) {
            assert_eq!(*r, Some(i as u32 * 2));
        }
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(map(&[] as &[u8], 4, |_| 0).is_empty());
    }
}
