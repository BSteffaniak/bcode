//! Structural resource accounting for bounded semantic observations.
//!
//! Counts describe owned scalar capacity, collection slots, semantic nodes,
//! nesting depth and simultaneously owned copies. They are not allocator-byte
//! estimates: in particular no size is assigned to a B-tree node.

/// Resource policy for an observation-only source subscription.
///
/// Scalar capacity, collection slots and nodes are aggregate bounds across queued
/// and in-flight events. Depth is a per-value bound. Queue and in-flight counts
/// are separate, so dequeueing cannot release an event's structural reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionObservationSourceLimits {
    /// Maximum aggregate owned scalar capacity.
    pub max_scalar_capacity_bytes: usize,
    /// Maximum aggregate retained collection slots.
    pub max_collection_entries: usize,
    /// Maximum aggregate semantic value nodes.
    pub max_value_nodes: usize,
    /// Maximum nesting depth of any admitted event.
    pub max_depth: usize,
    /// Maximum queued observation events.
    pub max_queued_events: usize,
    /// Maximum dequeued events whose leases remain live.
    pub max_in_flight_events: usize,
}

impl SessionObservationSourceLimits {
    /// Structural bounds corresponding to this subscription's aggregate policy.
    ///
    /// Each queued or in-flight event owns one copy reservation. An overflowing
    /// combined event count rejects the policy rather than saturating it.
    ///
    /// # Errors
    /// Returns an error if the combined event counts overflow.
    pub const fn retention_limits(
        self,
    ) -> Result<ObservationRetentionLimits, ObservationRetentionExceeded> {
        let Some(copies) = self
            .max_queued_events
            .checked_add(self.max_in_flight_events)
        else {
            return Err(ObservationRetentionExceeded);
        };
        Ok(ObservationRetentionLimits {
            scalar_capacity_bytes: self.max_scalar_capacity_bytes,
            collection_entries: self.max_collection_entries,
            value_nodes: self.max_value_nodes,
            depth: self.max_depth,
            copies,
        })
    }
}

/// Independent semantic resource limits for an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationRetentionLimits {
    /// Maximum owned string/byte-buffer capacity in bytes.
    pub scalar_capacity_bytes: usize,
    /// Maximum retained collection slots (vector capacity or map/set entries).
    pub collection_entries: usize,
    /// Maximum semantic value nodes.
    pub value_nodes: usize,
    /// Maximum nesting depth, counting a root value as depth one.
    pub depth: usize,
    /// Maximum simultaneously owned state/payload copies.
    pub copies: usize,
}

/// Structural usage, not an estimate of total allocator retention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservationRetentionUsage {
    /// Sum of owned string/byte-buffer capacities.
    pub scalar_capacity_bytes: usize,
    /// Sum of retained vector capacities and map/set entry counts.
    pub collection_entries: usize,
    /// Number of semantic value nodes.
    pub value_nodes: usize,
    /// Maximum depth of the retained values.
    pub depth: usize,
    /// Number of simultaneously owned state/payload copies.
    pub copies: usize,
}

/// A structural reservation could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationRetentionExceeded;

impl std::fmt::Display for ObservationRetentionExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("observation structural retention limit exceeded")
    }
}

impl std::error::Error for ObservationRetentionExceeded {}

impl ObservationRetentionUsage {
    /// Check each independent dimension without guessing allocator overhead.
    ///
    /// # Errors
    /// Returns an error if any dimension exceeds its limit.
    pub const fn check(
        self,
        limits: ObservationRetentionLimits,
    ) -> Result<(), ObservationRetentionExceeded> {
        if self.scalar_capacity_bytes > limits.scalar_capacity_bytes
            || self.collection_entries > limits.collection_entries
            || self.value_nodes > limits.value_nodes
            || self.depth > limits.depth
            || self.copies > limits.copies
        {
            Err(ObservationRetentionExceeded)
        } else {
            Ok(())
        }
    }

    /// Combine simultaneously owned values, preserving the maximum depth.
    ///
    /// # Errors
    /// Returns an error on counter overflow or an exceeded dimension. On error,
    /// the original usage is unchanged.
    pub fn try_add(
        &mut self,
        other: Self,
        limits: ObservationRetentionLimits,
    ) -> Result<(), ObservationRetentionExceeded> {
        let next = Self {
            scalar_capacity_bytes: self
                .scalar_capacity_bytes
                .checked_add(other.scalar_capacity_bytes)
                .ok_or(ObservationRetentionExceeded)?,
            collection_entries: self
                .collection_entries
                .checked_add(other.collection_entries)
                .ok_or(ObservationRetentionExceeded)?,
            value_nodes: self
                .value_nodes
                .checked_add(other.value_nodes)
                .ok_or(ObservationRetentionExceeded)?,
            depth: self.depth.max(other.depth),
            copies: self
                .copies
                .checked_add(other.copies)
                .ok_or(ObservationRetentionExceeded)?,
        };
        next.check(limits)?;
        *self = next;
        Ok(())
    }

    /// Release a previously admitted usage reservation.
    ///
    /// Depth is a conservative high-water mark until all value nodes are
    /// released. Lease owners must release each reservation exactly once.
    ///
    /// # Errors
    /// Returns an error on counter underflow, leaving usage unchanged.
    pub fn try_release(&mut self, other: Self) -> Result<(), ObservationRetentionExceeded> {
        let next = Self {
            scalar_capacity_bytes: self
                .scalar_capacity_bytes
                .checked_sub(other.scalar_capacity_bytes)
                .ok_or(ObservationRetentionExceeded)?,
            collection_entries: self
                .collection_entries
                .checked_sub(other.collection_entries)
                .ok_or(ObservationRetentionExceeded)?,
            value_nodes: self
                .value_nodes
                .checked_sub(other.value_nodes)
                .ok_or(ObservationRetentionExceeded)?,
            copies: self
                .copies
                .checked_sub(other.copies)
                .ok_or(ObservationRetentionExceeded)?,
            depth: self.depth,
        };
        *self = Self {
            depth: if next.value_nodes == 0 { 0 } else { next.depth },
            ..next
        };
        Ok(())
    }

    /// Clone a string only after admitting its new owned scalar allocation.
    ///
    /// The new string is obtained from a boxed string slice, so its exposed
    /// capacity equals its length. The borrowed source's spare capacity is not
    /// copied. This reserves only the new scalar/node, not an enclosing event
    /// copy lease. Allocation failure follows standard Rust allocation behavior.
    ///
    /// # Errors
    /// Returns an error before cloning if a limit or counter overflow prevents
    /// admission. On error neither the source nor usage is changed.
    pub fn try_clone_string(
        &mut self,
        value: &str,
        depth: usize,
        limits: ObservationRetentionLimits,
    ) -> Result<String, ObservationRetentionExceeded> {
        let charge = Self {
            scalar_capacity_bytes: value.len(),
            value_nodes: 1,
            depth,
            ..Self::default()
        };
        let mut next = *self;
        next.try_add(charge, limits)?;
        let owned = Box::<str>::from(value).into_string();
        *self = next;
        Ok(owned)
    }

    /// Admit and concatenate borrowed UTF-8 scalars without automatic growth.
    ///
    /// The staging byte slice has a fixed admitted length. Converting its boxed
    /// slice into a vector preserves that length as capacity; constructing the
    /// string consumes the vector without a second scalar allocation. Existing
    /// scalar owners must remain charged until the caller replaces/drops them.
    ///
    /// # Errors
    /// Returns an error before allocation on length overflow or denied admission.
    /// Usage remains unchanged on error.
    pub fn try_concat_string(
        &mut self,
        left: &str,
        right: &str,
        depth: usize,
        limits: ObservationRetentionLimits,
    ) -> Result<String, ObservationRetentionExceeded> {
        let len = left
            .len()
            .checked_add(right.len())
            .ok_or(ObservationRetentionExceeded)?;
        let mut next = *self;
        next.try_add(
            Self {
                scalar_capacity_bytes: len,
                value_nodes: 1,
                depth,
                ..Self::default()
            },
            limits,
        )?;
        let mut bytes = vec![0u8; len].into_boxed_slice();
        bytes[..left.len()].copy_from_slice(left.as_bytes());
        bytes[left.len()..].copy_from_slice(right.as_bytes());
        let text = String::from_utf8(bytes.into_vec()).map_err(|_| ObservationRetentionExceeded)?;
        *self = next;
        Ok(text)
    }

    /// Account a borrowed owned string, including unused scalar capacity.
    #[must_use]
    pub const fn string(value: &String, depth: usize) -> Self {
        Self {
            scalar_capacity_bytes: value.capacity(),
            value_nodes: 1,
            depth,
            collection_entries: 0,
            copies: 0,
        }
    }

    /// Account a collection's own slots, excluding its elements' nested values.
    /// Use vector capacity for a vector and entry count for a map or set.
    #[must_use]
    pub const fn collection(slots: usize, depth: usize) -> Self {
        Self {
            collection_entries: slots,
            value_nodes: 1,
            depth,
            scalar_capacity_bytes: 0,
            copies: 0,
        }
    }

    /// Inspect a borrowed JSON tree without cloning or serializing it.
    ///
    /// Array spare capacity and object-key string capacity are included. Object
    /// keys count as value nodes. The traversal stack is bounded by the configured
    /// depth; this is not an allocation-admitting JSON decoder.
    ///
    /// # Errors
    /// Returns an error as soon as a limit or counter overflow is encountered.
    pub fn json(
        value: &serde_json::Value,
        limits: ObservationRetentionLimits,
    ) -> Result<Self, ObservationRetentionExceeded> {
        enum Frame<'a> {
            Array(std::slice::Iter<'a, serde_json::Value>),
            Object(serde_json::map::Iter<'a>),
        }
        let mut usage = Self::default();
        let mut stack = Vec::new();
        let mut next = Some((value, 1usize));
        loop {
            if let Some((current, depth)) = next.take() {
                let own = match current {
                    serde_json::Value::String(text) => Self::string(text, depth),
                    serde_json::Value::Array(values) => Self::collection(values.capacity(), depth),
                    serde_json::Value::Object(values) => Self::collection(values.len(), depth),
                    _ => Self {
                        value_nodes: 1,
                        depth,
                        ..Self::default()
                    },
                };
                usage.try_add(own, limits)?;
                // Depth admission precedes allocating a traversal frame. The
                // frame contains an iterator, not a cloned child collection.
                match current {
                    serde_json::Value::Array(values) => stack.push(Frame::Array(values.iter())),
                    serde_json::Value::Object(values) => stack.push(Frame::Object(values.iter())),
                    _ => {}
                }
            }
            let depth = stack
                .len()
                .checked_add(1)
                .ok_or(ObservationRetentionExceeded)?;
            let Some(frame) = stack.last_mut() else {
                return Ok(usage);
            };
            next = match frame {
                Frame::Array(values) => values.next().map(|child| (child, depth)),
                Frame::Object(values) => match values.next() {
                    Some((key, child)) => {
                        usage.try_add(Self::string(key, depth), limits)?;
                        Some((child, depth))
                    }
                    None => None,
                },
            };
            if next.is_none() {
                stack.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ObservationRetentionLimits {
        ObservationRetentionLimits {
            scalar_capacity_bytes: 128,
            collection_entries: 32,
            value_nodes: 64,
            depth: 8,
            copies: 2,
        }
    }

    #[test]
    fn charges_capacity_not_length() {
        let mut text = String::with_capacity(256);
        text.push('x');
        let usage = ObservationRetentionUsage::string(&text, 1);
        assert_eq!(usage.scalar_capacity_bytes, text.capacity());
        assert!(usage.check(limits()).is_err());
        let mut array = Vec::with_capacity(64);
        array.push(serde_json::Value::Null);
        assert!(
            ObservationRetentionUsage::json(&serde_json::Value::Array(array), limits()).is_err()
        );
    }

    #[test]
    fn failed_reservation_preserves_counters_and_repeated_growth_is_bounded() {
        let mut usage = ObservationRetentionUsage::default();
        let item = ObservationRetentionUsage {
            value_nodes: 1,
            collection_entries: 1,
            ..ObservationRetentionUsage::default()
        };
        for _ in 0..32 {
            usage.try_add(item, limits()).unwrap();
        }
        let before = usage;
        assert!(usage.try_add(item, limits()).is_err());
        assert_eq!(usage, before);
        assert!(
            usage
                .try_add(
                    ObservationRetentionUsage {
                        value_nodes: usize::MAX,
                        ..ObservationRetentionUsage::default()
                    },
                    limits()
                )
                .is_err()
        );
        assert_eq!(usage, before);
    }

    #[test]
    fn json_nodes_keys_and_depth_are_independent_limits() {
        let value = serde_json::json!({ "a": [null, false] });
        let usage = ObservationRetentionUsage::json(&value, limits()).unwrap();
        assert_eq!(usage.value_nodes, 5);
        assert_eq!(usage.depth, 3);
        assert!(
            ObservationRetentionUsage::json(
                &value,
                ObservationRetentionLimits {
                    depth: 2,
                    ..limits()
                }
            )
            .is_err()
        );
        assert!(
            ObservationRetentionUsage::json(
                &value,
                ObservationRetentionLimits {
                    value_nodes: 4,
                    ..limits()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn release_is_atomic_and_final_drop_returns_usage_to_zero() {
        let mut usage = ObservationRetentionUsage::default();
        let item = ObservationRetentionUsage {
            scalar_capacity_bytes: 8,
            collection_entries: 2,
            value_nodes: 3,
            depth: 4,
            copies: 1,
        };
        usage.try_add(item, limits()).unwrap();
        let before = usage;
        assert!(
            usage
                .try_release(ObservationRetentionUsage {
                    copies: 2,
                    ..ObservationRetentionUsage::default()
                })
                .is_err()
        );
        assert_eq!(usage, before);
        usage.try_release(item).unwrap();
        assert_eq!(usage, ObservationRetentionUsage::default());
    }

    #[test]
    fn string_clone_admission_precedes_growth_and_ignores_source_spare_capacity() {
        let mut source = String::with_capacity(512);
        source.push_str("retained");
        let mut usage = ObservationRetentionUsage::default();
        let too_small = ObservationRetentionLimits {
            scalar_capacity_bytes: 7,
            ..limits()
        };
        assert!(usage.try_clone_string(&source, 2, too_small).is_err());
        assert_eq!(usage, ObservationRetentionUsage::default());
        let cloned = usage.try_clone_string(&source, 2, limits()).unwrap();
        assert_eq!(cloned, source);
        assert_eq!(cloned.capacity(), cloned.len());
        assert_eq!(usage.scalar_capacity_bytes, cloned.capacity());
        usage
            .try_release(ObservationRetentionUsage::string(&cloned, 2))
            .unwrap();
        drop(cloned);
        assert_eq!(usage, ObservationRetentionUsage::default());
    }

    #[test]
    fn fixed_concat_admits_overlap_before_allocating() {
        let source = String::from("abc");
        let mut usage = ObservationRetentionUsage::string(&source, 1);
        let before = usage;
        assert!(
            usage
                .try_concat_string(
                    &source,
                    "def",
                    1,
                    ObservationRetentionLimits {
                        scalar_capacity_bytes: 8,
                        ..limits()
                    }
                )
                .is_err()
        );
        assert_eq!(usage, before);
        let output = usage
            .try_concat_string(&source, "def", 1, limits())
            .unwrap();
        assert_eq!(output, "abcdef");
        assert_eq!(output.capacity(), output.len());
        assert_eq!(
            usage.scalar_capacity_bytes,
            source.capacity() + output.capacity()
        );
        usage
            .try_release(ObservationRetentionUsage::string(&source, 1))
            .unwrap();
        usage
            .try_release(ObservationRetentionUsage::string(&output, 1))
            .unwrap();
        assert_eq!(usage, ObservationRetentionUsage::default());
    }

    #[test]
    fn source_copy_limit_overflow_is_rejected() {
        let source = SessionObservationSourceLimits {
            max_scalar_capacity_bytes: 128,
            max_collection_entries: 32,
            max_value_nodes: 64,
            max_depth: 8,
            max_queued_events: usize::MAX,
            max_in_flight_events: 1,
        };
        assert!(source.retention_limits().is_err());
        let source = SessionObservationSourceLimits {
            max_queued_events: 1,
            ..source
        };
        assert_eq!(source.retention_limits().unwrap().copies, 2);
    }

    #[test]
    fn releasing_deep_value_keeps_watermark_until_empty() {
        let mut usage = ObservationRetentionUsage::default();
        let deep = ObservationRetentionUsage {
            value_nodes: 1,
            depth: 8,
            copies: 1,
            ..ObservationRetentionUsage::default()
        };
        let shallow = ObservationRetentionUsage {
            value_nodes: 1,
            depth: 2,
            copies: 1,
            ..ObservationRetentionUsage::default()
        };
        usage.try_add(deep, limits()).unwrap();
        usage.try_add(shallow, limits()).unwrap();
        usage.try_release(deep).unwrap();
        assert_eq!(usage.depth, 8);
        usage.try_release(shallow).unwrap();
        assert_eq!(usage, ObservationRetentionUsage::default());
    }

    #[test]
    fn copies_are_charged_independently() {
        let mut usage = ObservationRetentionUsage::default();
        let copy = ObservationRetentionUsage {
            copies: 1,
            ..ObservationRetentionUsage::default()
        };
        usage.try_add(copy, limits()).unwrap();
        usage.try_add(copy, limits()).unwrap();
        assert!(usage.try_add(copy, limits()).is_err());
    }
}
