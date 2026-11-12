//! Fenwick tree over block lengths: `O(log n)` item index ↔ block lookups and updates.

/// Prefix sums over non-negative block lengths.
#[derive(Debug, Clone, Default)]
pub struct Fenwick {
    tree: Vec<u64>,
    values: Vec<u64>,
}

impl Fenwick {
    /// Build from initial values.
    #[must_use]
    pub fn from_values(values: &[u64]) -> Self {
        let mut tree = vec![0; values.len() + 1];
        // Children of node `i` are all below `i`, so `tree[i]` is complete when it is reached.
        for (index, value) in values.iter().enumerate() {
            let i = index + 1;
            tree[i] += value;
            let parent = i + (i.isolate_lowest_one());
            if parent < tree.len() {
                let carried = tree[i];
                tree[parent] += carried;
            }
        }
        Self {
            tree,
            values: values.to_vec(),
        }
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.values.len()
    }

    /// Set the value at `index`.
    pub fn set(&mut self, index: usize, value: u64) {
        let old = self.values[index];
        if old == value {
            return;
        }
        self.values[index] = value;
        let mut i = index + 1;
        while i < self.tree.len() {
            self.tree[i] = self.tree[i] + value - old;
            i += i.isolate_lowest_one();
        }
    }

    /// Sum of entries `[0, index)`.
    #[must_use]
    pub fn prefix(&self, index: usize) -> u64 {
        let mut i = index;
        let mut sum = 0;
        while i > 0 {
            sum += self.tree[i];
            i -= i.isolate_lowest_one();
        }
        sum
    }

    /// Sum of all entries.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.prefix(self.values.len())
    }

    /// The entry containing position `target` (`prefix(entry) <= target < prefix(entry + 1)`),
    /// skipping empty entries. `None` when `target >= total()`.
    #[must_use]
    pub fn find(&self, target: u64) -> Option<usize> {
        if target >= self.total() {
            return None;
        }
        let mut position = 0;
        let mut remaining = target;
        let mut step = self.tree.len().next_power_of_two();
        while step > 0 {
            let next = position + step;
            if next < self.tree.len() && self.tree[next] <= remaining {
                position = next;
                remaining -= self.tree[next];
            }
            step >>= 1;
        }
        Some(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_and_prefix_agree_with_linear_scan() {
        let values = [0, 3, 0, 0, 5, 1, 0, 2];
        let mut tree = Fenwick::from_values(&values);
        let check = |tree: &Fenwick, values: &[u64]| {
            let mut sum = 0;
            for (index, value) in values.iter().enumerate() {
                assert_eq!(tree.prefix(index), sum);
                for offset in 0..*value {
                    assert_eq!(tree.find(sum + offset), Some(index));
                }
                sum += value;
            }
            assert_eq!(tree.find(sum), None);
        };
        check(&tree, &values);
        tree.set(2, 4);
        tree.set(4, 0);
        check(&tree, &[0, 3, 4, 0, 0, 1, 0, 2]);
    }
}
