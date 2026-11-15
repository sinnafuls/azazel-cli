//! Rectangle packing: MaxRects with the best-short-side-fit rule, no rotation.

/// Side of the first bin tried for every page.
const START_SIZE: u32 = 64;

/// A rectangle to place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Top-left corner given to one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Index into the items passed to [`pack`].
    pub item: usize,
    pub x: u32,
    pub y: u32,
}

/// One square page and the items on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bin {
    /// Width and height in pixels.
    pub size: u32,
    pub placements: Vec<Placement>,
}

/// Places every item, leaving at least `padding` pixels between any two on the same page (none
/// against the page edge).
///
/// Items are placed tallest first, ties by name, so equal input always gives equal output. A page
/// starts 64 pixels square (less when `max_size` is smaller) and doubles, up to `max_size`, until
/// everything left fits; whatever does not fit a `max_size` page goes to the next page. An item
/// that cannot fit an empty `max_size` page is an error naming it.
pub fn pack(items: &[Item], padding: u32, max_size: u32) -> Result<Vec<Bin>, String> {
    if max_size == 0 {
        return Err("max_size must be at least 1".to_string());
    }
    if let Some(item) = items.iter().find(|item| item.width == 0 || item.height == 0) {
        return Err(format!("frame {} has no pixels", item.name));
    }
    if let Some(item) = items
        .iter()
        .find(|item| item.width > max_size || item.height > max_size)
    {
        return Err(format!(
            "frame {} is {}x{}, larger than max_size {max_size}",
            item.name, item.width, item.height
        ));
    }
    let mut remaining: Vec<usize> = (0..items.len()).collect();
    remaining.sort_by(|&a, &b| {
        let (a, b) = (&items[a], &items[b]);
        b.height.cmp(&a.height).then_with(|| a.name.cmp(&b.name))
    });

    let mut bins = Vec::new();
    while !remaining.is_empty() {
        let mut size = START_SIZE.min(max_size);
        loop {
            let (placements, left) = fill(items, &remaining, size, padding);
            if left.is_empty() || size == max_size {
                bins.push(Bin { size, placements });
                remaining = left;
                break;
            }
            size = size.saturating_mul(2).min(max_size);
        }
    }
    Ok(bins)
}

/// Places as many of `order` as fit one `size` × `size` page, trying them in order; returns the
/// placements and the items left over.
fn fill(items: &[Item], order: &[usize], size: u32, padding: u32) -> (Vec<Placement>, Vec<usize>) {
    // Growing every item and the page by the padding keeps items apart while letting them touch
    // the page edge.
    let side = size.saturating_add(padding);
    let mut page = MaxRects::new(side);
    let mut placements = Vec::new();
    let mut left = Vec::new();
    for &index in order {
        let item = &items[index];
        let spot = page.insert(
            item.width.saturating_add(padding),
            item.height.saturating_add(padding),
        );
        match spot {
            Some((x, y)) => placements.push(Placement { item: index, x, y }),
            None => left.push(index),
        }
    }
    (placements, left)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn right(&self) -> u32 {
        self.x + self.w
    }

    fn bottom(&self) -> u32 {
        self.y + self.h
    }

    fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.right() && other.x < self.right() && self.y < other.bottom() && other.y < self.bottom()
    }

    fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }
}

/// The free space of one page as a list of maximal, possibly overlapping rectangles.
struct MaxRects {
    free: Vec<Rect>,
}

impl MaxRects {
    fn new(side: u32) -> Self {
        MaxRects {
            free: vec![Rect {
                x: 0,
                y: 0,
                w: side,
                h: side,
            }],
        }
    }

    /// Claims a `w` × `h` rectangle in the free rectangle that leaves the smallest gap along its
    /// shorter leftover side (then along the longer one; the earliest free rectangle wins a tie),
    /// and returns its top-left corner. `None` when no free rectangle holds it.
    fn insert(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32, Rect)> = None;
        for free in &self.free {
            if free.w < w || free.h < h {
                continue;
            }
            let (spare_w, spare_h) = (free.w - w, free.h - h);
            let (short, long) = (spare_w.min(spare_h), spare_w.max(spare_h));
            if best.is_none_or(|(best_short, best_long, _)| (short, long) < (best_short, best_long)) {
                best = Some((short, long, *free));
            }
        }
        let (_, _, spot) = best?;
        let used = Rect {
            x: spot.x,
            y: spot.y,
            w,
            h,
        };
        self.split(&used);
        self.prune();
        Some((used.x, used.y))
    }

    /// Replaces every free rectangle touching `used` by the up to four maximal parts of it that
    /// lie left of, right of, above and below `used`.
    fn split(&mut self, used: &Rect) {
        let mut next = Vec::with_capacity(self.free.len() + 4);
        for free in &self.free {
            if !free.overlaps(used) {
                next.push(*free);
                continue;
            }
            if used.x > free.x {
                next.push(Rect {
                    w: used.x - free.x,
                    ..*free
                });
            }
            if used.right() < free.right() {
                next.push(Rect {
                    x: used.right(),
                    w: free.right() - used.right(),
                    ..*free
                });
            }
            if used.y > free.y {
                next.push(Rect {
                    h: used.y - free.y,
                    ..*free
                });
            }
            if used.bottom() < free.bottom() {
                next.push(Rect {
                    y: used.bottom(),
                    h: free.bottom() - used.bottom(),
                    ..*free
                });
            }
        }
        self.free = next;
    }

    /// Drops every free rectangle that lies inside another one.
    fn prune(&mut self) {
        let mut i = 0;
        while i < self.free.len() {
            let mut covered = false;
            let mut j = i + 1;
            while j < self.free.len() {
                if self.free[j].contains(&self.free[i]) {
                    covered = true;
                    break;
                }
                if self.free[i].contains(&self.free[j]) {
                    self.free.remove(j);
                } else {
                    j += 1;
                }
            }
            if covered {
                self.free.remove(i);
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(sizes: &[(u32, u32)]) -> Vec<Item> {
        sizes
            .iter()
            .enumerate()
            .map(|(index, &(width, height))| Item {
                name: format!("f{index:02}"),
                width,
                height,
            })
            .collect()
    }

    #[test]
    fn tallest_item_takes_the_origin_and_names_break_ties() {
        let mut list = items(&[(8, 8), (8, 20), (8, 8)]);
        list[0].name = "z".into();
        list[2].name = "a".into();
        let bins = pack(&list, 0, 4096).unwrap();
        let order: Vec<usize> = bins[0].placements.iter().map(|p| p.item).collect();
        assert_eq!(order, [1, 2, 0]);
        assert_eq!((bins[0].placements[0].x, bins[0].placements[0].y), (0, 0));
    }

    #[test]
    fn an_item_exactly_as_large_as_the_page_fits_despite_padding() {
        let bins = pack(&items(&[(64, 64)]), 8, 64).unwrap();
        assert_eq!(bins.len(), 1);
        assert_eq!(bins[0].size, 64);
        assert_eq!(bins[0].placements, [Placement { item: 0, x: 0, y: 0 }]);
    }

    #[test]
    fn empty_items_and_a_zero_max_size_are_refused() {
        assert_eq!(
            pack(&items(&[(0, 4)]), 0, 64).unwrap_err(),
            "frame f00 has no pixels"
        );
        assert!(pack(&items(&[(4, 4)]), 0, 0).is_err());
    }
}
