//! Bounded exact-adjacency prefetch for two already-required native boundary leaves.
//! All returned rasters, decoding and numerical consumption remain in the ordinary path.
use super::*;

impl SkvSource {
    // Build only the ordinary reader's exact physical demand ranges. This
    // bounded prepass never materializes windows or decodes payloads. Large
    // windows stay on the existing path rather than growing a global plan.
    pub(super) fn prepare_native_window(
        &self,
        window: [usize; 4],
        bands: &[usize],
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !crate::io::concurrent_transport_enabled()
            || !matches!(&self.state.borrow().store, Store::Remote(s) if s.cache_byte_capacity() > 0)
        {
            return Ok(());
        }
        let [x, y, w, h] = window;
        let edge = self.header.chunk_edge;
        let nx = self.metadata.grid.width.div_ceil(edge);
        let tiles = ((x + w - 1) / edge - x / edge + 1)
            .checked_mul((y + h - 1) / edge - y / edge + 1)
            .context("SKV plan overflow")?;
        if tiles > 128 || (tiles == 1 && bands.len() == 1) {
            return Ok(());
        }
        let mut limit = self.state.borrow().store.raw_range_limit();
        if self.header.grouped() {
            limit = limit.min(group::MAX_BYTES);
        }
        let mut demands = Vec::with_capacity(128);
        let (mut offset, mut length, mut count) = (0, 0usize, 0usize);
        for ty in y / edge..=(y + h - 1) / edge {
            for tx in x / edge..=(x + w - 1) / edge {
                let mut selected = self
                    .selected_leaf_records(ty * nx + tx, bands, cancel)?
                    .into_iter();
                while let Some(entry) = selected.next() {
                    let members =
                        selected_group_members(self.header.grouped(), &entry, selected.as_slice());
                    let size = u32_at(&entry.record, 8) as usize;
                    ensure!(
                        size <= limit,
                        "SKV payload exceeds per-request byte budget (configured range limit)"
                    );
                    if count > 0
                        && !pending_range_fits(offset, length, count, &entry, members, limit)
                    {
                        if demands.len() == 128 {
                            return Ok(());
                        }
                        demands.push((offset, length as u64));
                        count = 0;
                    }
                    if count == 0 {
                        offset = u64_at(&entry.record, 0);
                        length = 0;
                    }
                    length += size;
                    count += members;
                    for _ in 1..members {
                        selected.next().expect("counted group member");
                    }
                }
            }
        }
        if count > 0 {
            if demands.len() == 128 {
                return Ok(());
            }
            demands.push((offset, length as u64));
        }
        demands.sort_unstable();
        demands.dedup();
        if demands.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
            return Ok(());
        }
        let mut state = self.state.borrow_mut();
        if let Store::Remote(source) = &mut state.store {
            // No decoder/output coexist here. Existing8MiB per-read scratch,
            // minus descriptors/directory/planner overhead; cache is unchanged.
            source.prefetch_exact_ranges(&demands, (8 << 20) - (256 << 10), cancel)?;
        }
        Ok(())
    }

    pub(super) fn selected_leaf_records(
        &self,
        tile: usize,
        bands: &[usize],
        cancel: &AtomicBool,
    ) -> Result<Vec<Pending>> {
        let bounds = self.bounds(tile)?;
        let mut selected = Vec::with_capacity(bands.len());
        for (out_index, &band) in bands.iter().enumerate() {
            let id = tile * self.header.bands() + self.mapping[band];
            selected.push(Pending {
                out_index,
                band,
                id,
                bounds,
                record: self.record(id, cancel)?,
            });
        }
        selected.sort_unstable_by_key(|entry| u64_at(&entry.record, 0));
        Ok(selected)
    }

    // Produce the *whole* ranges the ordinary single-leaf window call will ask
    // for. A later request must fit in one cached response, not several pieces.
    fn leaf_demand_ranges(
        &self,
        tile: usize,
        bands: &[usize],
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<(u64, u64)>> {
        let mut selected = self.selected_leaf_records(tile, bands, cancel)?.into_iter();
        let mut ranges = Vec::with_capacity(bands.len());
        let (mut offset, mut length, mut count) = (0, 0usize, 0usize);
        while let Some(entry) = selected.next() {
            let members =
                selected_group_members(self.header.grouped(), &entry, selected.as_slice());
            let next_length = u32_at(&entry.record, 8) as usize;
            ensure!(
                next_length <= limit,
                "SKV payload exceeds per-request byte budget (configured range limit)"
            );
            if count > 0 && !pending_range_fits(offset, length, count, &entry, members, limit) {
                ranges.push((offset, length as u64));
                count = 0;
            }
            if count == 0 {
                offset = u64_at(&entry.record, 0);
                length = 0;
            }
            length += next_length;
            count += members;
            for _ in 1..members {
                selected.next().expect("counted canonical group member");
            }
        }
        if count > 0 {
            ranges.push((offset, length as u64));
        }
        Ok(ranges)
    }

    pub(super) fn prepare_boundary_pair(
        &self,
        windows: &[[usize; 4]],
        bands: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.guarded(|| {
            check_cancel(cancel)?;
            self.state.borrow().store.verify_local()?;
            if !self.boundary_prefetch_enabled() || windows.len() != 2 {
                return Ok(());
            }
            self.state.borrow_mut().metrics.boundary_prefetch_calls += 1;
            // Reuse the existing per-read8MiB scratch reservation. Includes one
            // selected-leaf descriptor vector, all demand ranges, directory page
            // input/copies and the transport planner plus largest response. The
            // response-cache capacity remains in retained_memory_bound(), unchanged.
            const RESERVE: usize = 8 << 20;
            const DESCRIPTORS: usize = 128 * std::mem::size_of::<Pending>()
                + 256 * std::mem::size_of::<(u64, u64)>()
                + PAGE * 10
                + 65_536;
            const _: () = assert!(DESCRIPTORS < 256 << 10);
            if max_bytes < RESERVE {
                return Ok(());
            }
            let started = Instant::now();
            let edge = self.header.chunk_edge;
            let nx = self.metadata.grid.width.div_ceil(edge);
            let mut tiles = [0usize; 2];
            for (i, &[x, y, width, height]) in windows.iter().enumerate() {
                self.validate_window(x, y, width, height, bands)?;
                if x % edge != 0
                    || y % edge != 0
                    || self.read_buffer_bound(width, height, bands)? > max_bytes
                {
                    return Ok(());
                }
                tiles[i] = y / edge * nx + x / edge;
                if self.bounds(tiles[i])? != [x, y, x + width, y + height] {
                    return Ok(());
                }
            }
            // Cheap conservative eligibility: unrelated leaves cannot be
            // adjacent in the qualified tile/band serving layouts. Descriptors
            // remain authoritative; no physical adjacency is inferred from this.
            if tiles[0].abs_diff(tiles[1]) != 1 {
                return Ok(());
            }
            let mut limit = self.state.borrow().store.raw_range_limit();
            if self.header.grouped() {
                limit = limit.min(group::MAX_BYTES);
            }
            let mut demands = Vec::with_capacity(bands.len() * 2);
            for tile in tiles {
                demands.extend(self.leaf_demand_ranges(tile, bands, limit, cancel)?);
            }
            demands.sort_unstable();
            demands.dedup();
            ensure!(
                demands.len() <= 128,
                "SKV boundary prefetch descriptor budget exceeded"
            );
            check_cancel(cancel)?;
            let mut state = self.state.borrow_mut();
            state.metrics.boundary_prefetch_plan_ms += started.elapsed().as_secs_f64() * 1000.;
            // Extra preparation calls cannot consume headroom reserved for the
            // subsequent ordinary demands. No physical/logical limit is raised.
            if state
                .metrics
                .logical_reads
                .saturating_add(demands.len() * 2)
                > MAX_READS
            {
                return Ok(());
            }
            let begin_ms = state.started.elapsed().as_secs_f64() * 1000.;
            let Store::Remote(remote) = &mut state.store else {
                return Ok(());
            };
            let prepared = remote.prefetch_exact_ranges(&demands, RESERVE - DESCRIPTORS, cancel)?;
            state.metrics.boundary_prefetch_plan_ms += prepared.planning_ms;
            state.metrics.boundary_prefetch_fetch_ms += prepared.fetch_ms;
            state.metrics.boundary_prefetch_admissions += usize::from(prepared.admitted);
            state.metrics.boundary_prefetch_planned_request_savings +=
                prepared.planned_request_savings;
            if prepared.admitted {
                state.metrics.boundary_prefetch_scratch_bound_bytes = RESERVE;
                state
                    .metrics
                    .boundary_prefetch_demand_cache_charge_peak_bytes = state
                    .metrics
                    .boundary_prefetch_demand_cache_charge_peak_bytes
                    .max(prepared.new_cache_charge_bytes + prepared.protected_cache_charge_bytes);
            }
            for (offset, length, relative_ms, duration_ms) in prepared.events {
                state.metrics.logical_reads += 1;
                state.metrics.logical_bytes += length;
                state.metrics.read_ms += duration_ms;
                state.metrics.boundary_prefetch_ranges += 1;
                state.metrics.boundary_prefetch_encoded_bytes += length;
                if state.metrics.records.len() < 4096 {
                    state.metrics.records.push(ReadRecord {
                        kind: "raw_prefetch",
                        offset,
                        length: length as usize,
                        start_ms: begin_ms + relative_ms,
                        duration_ms,
                    });
                }
            }
            ensure!(
                state
                    .store
                    .remote_metrics()
                    .is_none_or(|m| m.ranges.capacity() <= MAX_READS),
                "SKV remote request log capacity exceeds admission"
            );
            check_cancel(cancel)?;
            Ok(())
        })
    }
}
