use super::*;

pub struct Updater {
  cache: HashMap<[u8; 36], Vec<u8>>,
  outputs_traversed: u64,
  outputs_cached: u64,
  outputs_inserted_since_flush: u64,
  height: u64,
}

impl Updater {
  pub(crate) fn update(index: &Index) -> Result {
    let wtx = index.begin_write()?;

    let height = wtx
      .open_table(HEIGHT_TO_BLOCK_HASH)?
      .range(0..)?
      .rev()
      .next()
      .map(|(height, _hash)| height + 1)
      .unwrap_or(0);

    let mut updater = Self {
      cache: HashMap::new(),
      outputs_traversed: 0,
      outputs_cached: 0,
      outputs_inserted_since_flush: 0,
      height,
    };

    updater.update_index(index, wtx)
  }

  pub(crate) fn height(&self) -> u64 {
    self.height
  }

  fn flush(&mut self, wtx: &mut WriteTransaction) -> Result {
    log::info!(
      "Flushing {} entries ({:.1}% resulting from {} insertions) from memory to database",
      self.cache.len(),
      self.cache.len() as f64 / self.outputs_inserted_since_flush as f64 * 100.,
      self.outputs_inserted_since_flush,
    );
    let mut outpoint_to_ordinal_ranges = wtx.open_table(OUTPOINT_TO_ORDINAL_RANGES)?;

    for (k, v) in &self.cache {
      outpoint_to_ordinal_ranges.insert(k, v)?;
    }

    self.cache.clear();
    self.outputs_inserted_since_flush = 0;
    Ok(())
  }

  pub(crate) fn get_and_remove(
    &mut self,
    outpoint: OutPoint,
    outpoint_to_ordinal_ranges: &mut Table<[u8; 36], [u8]>,
  ) -> Result<Vec<u8>> {
    let key = encode_outpoint(outpoint);
    match self.cache.remove(&key) {
      Some(ord_range_vec) => {
        self.outputs_cached += 1;
        Ok(ord_range_vec)
      }
      None => {
        let ord_range = outpoint_to_ordinal_ranges
          .remove(&key)?
          .ok_or_else(|| anyhow!("Could not find outpoint {} in index", outpoint))?;
        Ok(ord_range.to_value().to_vec())
      }
    }
  }

  pub(crate) fn insert(&mut self, outpoint: &mut OutPoint, ordinals: Vec<u8>) {
    let key = encode_outpoint(*outpoint);
    self.cache.insert(key, ordinals);
    self.outputs_inserted_since_flush += 1;
  }

  pub(crate) fn commit(&mut self, mut wtx: WriteTransaction) -> Result {
    log::info!(
      "Committing at block height {}, {} outputs traversed, {} in map, {} cached",
      self.height,
      self.outputs_traversed,
      self.cache.len(),
      self.outputs_cached
    );

    self.flush(&mut wtx)?;

    Index::increment_statistic(&wtx, Statistic::OutputsTraversed, self.outputs_traversed)?;
    Index::increment_statistic(&wtx, Statistic::Commits, 1)?;
    wtx.commit()?;
    Ok(())
  }

  pub(crate) fn update_index<'index>(
    &mut self,
    index: &'index Index,
    mut wtx: WriteTransaction<'index>,
  ) -> Result {
    let mut progress_bar = if cfg!(test) || log_enabled!(log::Level::Info) {
      None
    } else {
      let progress_bar = ProgressBar::new(index.client.get_block_count()?);
      progress_bar.set_position(self.height());
      progress_bar.set_style(
        ProgressStyle::with_template("[indexing blocks] {wide_bar} {pos}/{len}").unwrap(),
      );
      Some(progress_bar)
    };

    let mut uncomitted = 0;
    for i in 0.. {
      if let Some(height_limit) = index.height_limit {
        if self.height() > height_limit {
          break;
        }
      }

      let done = self.index_block(index, &mut wtx)?;

      if !done {
        if let Some(progress_bar) = &mut progress_bar {
          progress_bar.inc(1);

          if progress_bar.position() > progress_bar.length().unwrap() {
            progress_bar.set_length(index.client.get_block_count()?);
          }
        }

        uncomitted += 1;
      }

      if uncomitted > 0 && i % 5000 == 0 {
        self.commit(wtx)?;
        wtx = index.begin_write()?;
        uncomitted = 0;
      }

      if done || INTERRUPTS.load(atomic::Ordering::Relaxed) > 0 {
        break;
      }
    }

    if uncomitted > 0 {
      self.commit(wtx)?;
    }

    if let Some(progress_bar) = &mut progress_bar {
      progress_bar.finish_and_clear();
    }

    Ok(())
  }

  pub(crate) fn index_block(&mut self, index: &Index, wtx: &mut WriteTransaction) -> Result<bool> {
    let mut height_to_block_hash = wtx.open_table(HEIGHT_TO_BLOCK_HASH)?;
    let mut ordinal_to_satpoint = wtx.open_table(ORDINAL_TO_SATPOINT)?;
    let mut outpoint_to_ordinal_ranges = wtx.open_table(OUTPOINT_TO_ORDINAL_RANGES)?;

    let start = Instant::now();
    let mut ordinal_ranges_written = 0;
    let mut outputs_in_block = 0;

    let block = match index.block_with_retries(self.height)? {
      Some(block) => block,
      None => return Ok(true),
    };

    let time: DateTime<Utc> = DateTime::from_utc(
      NaiveDateTime::from_timestamp(block.header.time as i64, 0),
      Utc,
    );

    log::info!(
      "Block {} at {} with {} transactions…",
      self.height,
      time,
      block.txdata.len()
    );

    if let Some(prev_height) = self.height.checked_sub(1) {
      let prev_hash = height_to_block_hash.get(&prev_height)?.unwrap();

      if prev_hash != block.header.prev_blockhash.as_ref() {
        index.reorged.store(true, Ordering::Relaxed);
        return Err(anyhow!("reorg detected at or before {prev_height}"));
      }
    }

    let mut coinbase_inputs = VecDeque::new();

    let h = Height(self.height);
    if h.subsidy() > 0 {
      let start = h.starting_ordinal();
      coinbase_inputs.push_front((start.n(), (start + h.subsidy()).n()));
    }

    let txdata = block
      .txdata
      .par_iter()
      .map(|tx| (tx.txid(), tx))
      .collect::<Vec<(Txid, &Transaction)>>();

    for (tx_offset, (txid, tx)) in txdata.iter().enumerate().skip(1) {
      log::trace!("Indexing transaction {tx_offset}…");

      let mut input_ordinal_ranges = VecDeque::new();

      for input in &tx.input {
        let ordinal_ranges =
          self.get_and_remove(input.previous_output, &mut outpoint_to_ordinal_ranges);

        for chunk in ordinal_ranges?.chunks_exact(11) {
          input_ordinal_ranges.push_back(Index::decode_ordinal_range(chunk.try_into().unwrap()));
        }
      }

      self.index_transaction(
        *txid,
        tx,
        &mut ordinal_to_satpoint,
        &mut input_ordinal_ranges,
        &mut ordinal_ranges_written,
        &mut outputs_in_block,
      )?;

      coinbase_inputs.extend(input_ordinal_ranges);
    }

    if let Some((txid, tx)) = txdata.first() {
      self.index_transaction(
        *txid,
        tx,
        &mut ordinal_to_satpoint,
        &mut coinbase_inputs,
        &mut ordinal_ranges_written,
        &mut outputs_in_block,
      )?;
    }

    height_to_block_hash.insert(&self.height, &block.block_hash().as_hash().into_inner())?;

    self.height += 1;
    self.outputs_traversed += outputs_in_block;

    log::info!(
      "Wrote {ordinal_ranges_written} ordinal ranges from {outputs_in_block} outputs in {} ms",
      (Instant::now() - start).as_millis(),
    );

    Ok(false)
  }

  pub(crate) fn index_transaction(
    &mut self,
    txid: Txid,
    tx: &Transaction,
    ordinal_to_satpoint: &mut Table<u64, [u8; 44]>,
    input_ordinal_ranges: &mut VecDeque<(u64, u64)>,
    ordinal_ranges_written: &mut u64,
    outputs_traversed: &mut u64,
  ) -> Result {
    for (vout, output) in tx.output.iter().enumerate() {
      let mut outpoint = OutPoint {
        vout: vout as u32,
        txid,
      };
      let mut ordinals = Vec::new();

      let mut remaining = output.value;
      while remaining > 0 {
        let range = input_ordinal_ranges
          .pop_front()
          .ok_or_else(|| anyhow!("insufficient inputs for transaction outputs"))?;

        if !Ordinal(range.0).is_common() {
          ordinal_to_satpoint.insert(
            &range.0,
            &encode_satpoint(SatPoint {
              outpoint,
              offset: output.value - remaining,
            }),
          )?;
        }

        let count = range.1 - range.0;

        let assigned = if count > remaining {
          let middle = range.0 + remaining;
          input_ordinal_ranges.push_front((middle, range.1));
          (range.0, middle)
        } else {
          range
        };

        let base = assigned.0;
        let delta = assigned.1 - assigned.0;

        let n = base as u128 | (delta as u128) << 51;

        ordinals.extend_from_slice(&n.to_le_bytes()[0..11]);

        remaining -= assigned.1 - assigned.0;

        *ordinal_ranges_written += 1;
      }

      *outputs_traversed += 1;

      self.insert(&mut outpoint, ordinals);
    }

    Ok(())
  }
}
