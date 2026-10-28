//! Timing baseline for the pure tree/layout algorithms.
//!
//! Fixtures are built through the crate's public API only, with the shape a
//! real workspace reaches by repeated `insert_window`: every window lands as
//! a sibling in the root container. That is deliberate — this bench exists to
//! compare later optimization scopes against the same input, not to model
//! every tree shape a user can reach through `join`/`move`.
//!
//! Group ids are the contract for those comparisons: `layout/tiles/{n}`,
//! `layout/accordion/{n}`, `insert/{n}`, `remove/{n}`,
//! `focus/right/32`, and `balance/root/32`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tili_tree::{Direction, Gaps, Layout, NodeId, Orientation, Rect, Tree, WindowId};

/// A 14" MacBook Pro's default logical area — fixed so measurements don't
/// drift with the machine's current display configuration.
const AREA: Rect = Rect {
    x: 0.0,
    y: 0.0,
    width: 1512.0,
    height: 945.0,
};

fn gaps() -> Gaps {
    Gaps {
        inner: 8.0,
        outer: (8.0, 8.0, 8.0, 8.0),
        // Non-`None` so `layout`/`resize_handle_at` take the tiled-window
        // count path rather than the fast fallback.
        outer_solo: Some((12.0, 12.0, 12.0, 12.0)),
        accordion: 24.0,
    }
}

fn window_id(index: usize) -> WindowId {
    // One-based so a "window 1" id never collides with the zero slot.
    WindowId::try_from(index + 1).expect("fixture window count fits in u32")
}

/// Builds `n` windows as a flat root container in `layout`, plus each leaf's
/// `NodeId` in insertion order. One leaf is resized so `layout` exercises
/// non-uniform weight division, which is what a live workspace normally has
/// after any manual resize.
fn tree_with_windows(n: usize, layout: Layout) -> (Tree, Vec<NodeId>) {
    let mut tree = Tree::new();
    tree.set_default_layout(layout);

    let mut ids = Vec::with_capacity(n);
    let mut near = None;
    for index in 0..n {
        let node = tree.insert_window(window_id(index), near, Orientation::Horizontal);
        ids.push(node);
        near = Some(node);
    }
    if let Some(&last) = ids.last() {
        // A no-op for a lone window or an Accordion root; both are valid
        // fixtures, so the return value is intentionally ignored.
        tree.resize_weight(last, 0.1);
    }
    (tree, ids)
}

fn bench_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("layout");
    for n in [1, 2, 4, 8, 16, 32, 64] {
        // ns/window is the metric that makes different N values comparable.
        group.throughput(Throughput::Elements(
            u64::try_from(n).expect("fixture size fits in u64"),
        ));

        let (tiles, _) = tree_with_windows(n, Layout::Tiles);
        group.bench_with_input(BenchmarkId::new("tiles", n), &tiles, |b, tree| {
            b.iter(|| black_box(tree.layout(black_box(AREA), black_box(gaps()))));
        });

        let (accordion, _) = tree_with_windows(n, Layout::Accordion);
        group.bench_with_input(BenchmarkId::new("accordion", n), &accordion, |b, tree| {
            b.iter(|| black_box(tree.layout(black_box(AREA), black_box(gaps()))))
        });
    }
    group.finish();
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    for n in [8, 32, 64] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            // Fresh tree per batch: an insert mutates the root container, so
            // reusing one fixture would measure a tree that keeps growing.
            b.iter_batched(
                || tree_with_windows(n, Layout::Tiles).0,
                |mut tree| {
                    // The daemon inserts beside the focused (MRU) leaf.
                    let near = tree.default_focus();
                    black_box(tree.insert_window(
                        black_box(window_id(n)),
                        near,
                        Orientation::Horizontal,
                    ));
                },
                BatchSize::NumBatches(20),
            );
        });
    }
    group.finish();
}

fn bench_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("remove");
    for n in [8, 32, 64] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || tree_with_windows(n, Layout::Tiles).0,
                |mut tree| {
                    // Middle window, so the removal isn't always the last
                    // child (which skips most of the MRU fix-up).
                    black_box(tree.remove_window(black_box(window_id(n / 2))));
                },
                BatchSize::NumBatches(20),
            );
        });
    }
    group.finish();
}

fn bench_focus(c: &mut Criterion) {
    let (mut tree, ids) = tree_with_windows(32, Layout::Tiles);
    let from = ids[16];

    let mut group = c.benchmark_group("focus");
    group.bench_function("right/32", |b| {
        b.iter(|| black_box(tree.focus_in_direction(black_box(from), black_box(Direction::Right))));
    });
    group.finish();
}

fn bench_balance(c: &mut Criterion) {
    let mut group = c.benchmark_group("balance");
    group.bench_function("root/32", |b| {
        // Each batch starts from a skewed tree, so the measured call does
        // the same amount of work every time instead of re-flattening
        // weights that are already even.
        b.iter_batched(
            || {
                let (mut tree, ids) = tree_with_windows(32, Layout::Tiles);
                tree.resize_weight(ids[16], 0.2);
                (tree, ids[16])
            },
            |(mut tree, from)| black_box(tree.balance_weights(black_box(from), true)),
            BatchSize::NumBatches(50),
        );
    });
    group.finish();
}

criterion_group! {
    name = benches;
    // Short enough that all groups finish well under a minute; long enough
    // for 50 samples per id, which is criterion's own floor for stable output.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1000))
        .sample_size(50);
    targets = bench_layout, bench_insert, bench_remove, bench_focus, bench_balance
}
criterion_main!(benches);
