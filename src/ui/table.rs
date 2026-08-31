//! Fitting a wide table into a narrow window.
//!
//! `egui_extras` lays a table out left to right and simply runs off the edge
//! when the columns do not fit — silently, with no scrollbar. On a table with
//! sixteen columns that means the last few, including the row's action buttons,
//! become unreachable at anything below a very wide window. A horizontal
//! scrollbar would technically fix that, but scrolling sideways to press Stop
//! is a poor way to run a tunnel.
//!
//! So each column declares how narrow it is still useful at, and a drop rank:
//! when the window cannot hold everything, the least useful columns go first
//! and what remains still lines up. Rank 0 columns are never dropped, which is
//! how the identity of a row and its controls stay on screen at any width.
//! Anything dropped is still reachable — the row's detail panel and hover text
//! carry the same facts.

use egui_extras::Column;

/// Width the vertical scrollbar takes that `available_width` does not account
/// for. Without this the last column sits half under the scrollbar.
const SCROLLBAR_ALLOWANCE: f32 = 18.0;

/// One column, declared in the order it is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColSpec<K> {
    /// The caller's own identifier, matched on when filling header and cells.
    pub key: K,
    /// Width the column is drawn at. Fixed rather than content-sized, so that
    /// what fits is decided here rather than by whichever row happens to hold
    /// the longest hostname.
    pub width: f32,
    /// Drop order: the lowest rank above zero goes first, ties broken by the
    /// rightmost. Rank 0 is never dropped.
    pub rank: u8,
    /// Takes the leftover width instead of a fixed one. At most one column
    /// should set this, and it should be rank 0 — a table with no remainder
    /// column leaves a ragged gap on the right.
    pub grow: bool,
}

impl<K> ColSpec<K> {
    /// A column that is always shown.
    pub const fn keep(key: K, width: f32) -> Self {
        Self { key, width, rank: 0, grow: false }
    }

    /// A column that may be dropped when the window is too narrow. `rank`
    /// orders the sacrifices: 1 goes first.
    pub const fn opt(key: K, width: f32, rank: u8) -> Self {
        Self { key, width, rank, grow: false }
    }

    /// Take the remaining width. Implies the column is kept.
    pub const fn grow(mut self) -> Self {
        self.grow = true;
        self.rank = 0;
        self
    }

    /// The `egui_extras` column this describes.
    ///
    /// Always clipped. These widths are fixed, so content that does not fit
    /// has nowhere to go: unclipped it paints straight over the neighbouring
    /// cell, which is how a long hostname ends up sitting on top of a latency
    /// reading. Clipped, it is merely cut off — and every cell that can
    /// overflow carries the whole value in its hover.
    pub fn column(&self) -> Column {
        if self.grow {
            Column::remainder().at_least(self.width).clip(true)
        } else {
            Column::exact(self.width).clip(true)
        }
    }
}

/// The columns of `specs` that fit in `avail`, in declaration order.
///
/// `avail` is `ui.available_width()`; `spacing` is `ui.spacing().item_spacing.x`.
pub fn fit<K: Copy>(specs: &[ColSpec<K>], avail: f32, spacing: f32) -> Vec<ColSpec<K>> {
    // Before the first layout pass egui can report a width of zero or infinity.
    // Showing everything is the better guess there: it is one frame, and a
    // table that drops all its columns for a frame flickers.
    if !avail.is_finite() || avail <= 0.0 {
        return specs.to_vec();
    }
    let budget = avail - SCROLLBAR_ALLOWANCE;

    let mut keep = vec![true; specs.len()];
    loop {
        let n = keep.iter().filter(|k| **k).count();
        let used: f32 =
            specs.iter().zip(&keep).filter(|(_, k)| **k).map(|(c, _)| c.width).sum::<f32>()
                + spacing * (n.saturating_sub(1)) as f32;
        if used <= budget {
            break;
        }
        // The cheapest column to lose: lowest rank, and among equals the one
        // furthest right, so a pair like the two rate columns collapses from
        // the outside in.
        let victim = specs
            .iter()
            .enumerate()
            .filter(|(i, c)| keep[*i] && c.rank > 0)
            .min_by_key(|(i, c)| (c.rank, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match victim {
            Some(i) => keep[i] = false,
            // Only rank-0 columns left. They stay: losing the name or the
            // buttons would make the row useless, and clipping is survivable.
            None => break,
        }
    }

    specs.iter().zip(keep).filter(|(_, k)| *k).map(|(c, _)| *c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum C {
        Dot,
        Name,
        Addr,
        Cheap,
        Dear,
        Act,
    }

    fn specs() -> Vec<ColSpec<C>> {
        vec![
            ColSpec::keep(C::Dot, 20.0),
            ColSpec::keep(C::Name, 100.0),
            ColSpec::opt(C::Cheap, 50.0, 1),
            ColSpec::opt(C::Dear, 50.0, 2),
            ColSpec::keep(C::Addr, 100.0).grow(),
            ColSpec::keep(C::Act, 80.0),
        ]
    }

    fn keys(v: &[ColSpec<C>]) -> Vec<C> {
        v.iter().map(|c| c.key).collect()
    }

    #[test]
    fn everything_fits_when_there_is_room() {
        let got = fit(&specs(), 2000.0, 8.0);
        assert_eq!(keys(&got), vec![C::Dot, C::Name, C::Cheap, C::Dear, C::Addr, C::Act]);
    }

    /// The whole point: the row's buttons and its identity survive any width.
    /// If this ever fails, the app has a size at which tunnels cannot be
    /// stopped.
    #[test]
    fn the_kept_columns_survive_even_an_absurd_width() {
        for w in [40.0, 120.0, 300.0, 380.0] {
            let got = keys(&fit(&specs(), w, 8.0));
            assert_eq!(got, vec![C::Dot, C::Name, C::Addr, C::Act], "at {w}px");
        }
    }

    #[test]
    fn the_least_useful_column_is_the_first_to_go() {
        // Room for one optional column only.
        let all: f32 = 20.0 + 100.0 + 100.0 + 80.0 + 50.0 + 5.0 * 8.0 + SCROLLBAR_ALLOWANCE;
        let got = keys(&fit(&specs(), all, 8.0));
        assert_eq!(got, vec![C::Dot, C::Name, C::Dear, C::Addr, C::Act], "rank 1 goes first");
    }

    #[test]
    fn columns_keep_their_declared_order_after_dropping() {
        let got = fit(&specs(), 500.0, 8.0);
        let order: Vec<usize> =
            got.iter().map(|c| specs().iter().position(|s| s.key == c.key).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "got {order:?}");
    }

    /// A width egui has not measured yet must not throw the layout away for a
    /// frame — that reads as a flicker every time the tab is opened.
    #[test]
    fn an_unmeasured_width_shows_everything() {
        assert_eq!(fit(&specs(), 0.0, 8.0).len(), 6);
        assert_eq!(fit(&specs(), f32::INFINITY, 8.0).len(), 6);
        assert_eq!(fit(&specs(), f32::NAN, 8.0).len(), 6);
    }

    #[test]
    fn ties_drop_from_the_right() {
        let s = vec![
            ColSpec::keep(C::Name, 100.0),
            ColSpec::opt(C::Cheap, 50.0, 3),
            ColSpec::opt(C::Dear, 50.0, 3),
        ];
        let width = 100.0 + 50.0 + 8.0 + SCROLLBAR_ALLOWANCE;
        assert_eq!(keys(&fit(&s, width, 8.0)), vec![C::Name, C::Cheap]);
    }
}
