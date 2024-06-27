// Copyright 2023 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bincode::config::{FixintEncoding, WithOtherIntEncoding};
use bincode::{DefaultOptions, Options};
use futures::stream::{self, FuturesOrdered, StreamExt, TryStreamExt};
use nativelink_error::{make_err, Code, Error, ResultExt};
use nativelink_util::buf_channel::{DropCloserReadHalf, DropCloserWriteHalf};
use nativelink_util::common::DigestInfo;
use nativelink_util::fastcdc::FastCDC;
use nativelink_util::health_utils::{default_health_status_indicator, HealthStatusIndicator};
use nativelink_util::store_trait::{Store, StoreDriver, StoreKey, StoreLike, UploadSizeInfo};
use serde::{Deserialize, Serialize};
use tokio_util::codec::FramedRead;
use tokio_util::io::StreamReader;
use tracing::{event, Level};

// NOTE: If these change update the comments in `stores.rs` to reflect
// the new defaults.
const DEFAULT_MIN_SIZE: usize = 64 * 1024;
const DEFAULT_NORM_SIZE: usize = 256 * 1024;
const DEFAULT_MAX_SIZE: usize = 512 * 1024;
const DEFAULT_MAX_CONCURRENT_FETCH_PER_GET: usize = 10;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub enum DedupEntry {
    Base(DigestInfo),
    Delta(DigestInfo, DigestInfo, i64),
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Default, Clone)]
pub struct DedupIndex {
    pub entries: Vec<DedupEntry>,
}

pub struct DedupStore {
    lsh_store: Store,
    index_store: Store,
    content_store: Store,
    fast_cdc_decoder: FastCDC,
    max_concurrent_fetch_per_get: usize,
    bincode_options: WithOtherIntEncoding<DefaultOptions, FixintEncoding>,
}

impl DedupStore {
    pub fn new(
        config: &nativelink_config::stores::DedupStore,
        lsh_store: Store,
        index_store: Store,
        content_store: Store,
    ) -> Arc<Self> {
        let min_size = if config.min_size == 0 {
            DEFAULT_MIN_SIZE
        } else {
            config.min_size as usize
        };
        let normal_size = if config.normal_size == 0 {
            DEFAULT_NORM_SIZE
        } else {
            config.normal_size as usize
        };
        let max_size = if config.max_size == 0 {
            DEFAULT_MAX_SIZE
        } else {
            config.max_size as usize
        };
        let max_concurrent_fetch_per_get = if config.max_concurrent_fetch_per_get == 0 {
            DEFAULT_MAX_CONCURRENT_FETCH_PER_GET
        } else {
            config.max_concurrent_fetch_per_get as usize
        };
        Arc::new(Self {
            lsh_store,
            index_store,
            content_store,
            fast_cdc_decoder: FastCDC::new(min_size, normal_size, max_size),
            max_concurrent_fetch_per_get,
            bincode_options: DefaultOptions::new().with_fixint_encoding(),
        })
    }

    async fn has(self: Pin<&Self>, key: StoreKey<'_>) -> Result<Option<usize>, Error> {
        // First we need to load the index that contains where the individual parts actually
        // can be fetched from.
        let index_entries = {
            let maybe_data = self
                .index_store
                .get_part_unchunked(key.borrow(), 0, None)
                .await
                .err_tip(|| "Failed to read index store in dedup store");
            let data = match maybe_data {
                Err(e) => {
                    if e.code == Code::NotFound {
                        return Ok(None);
                    }
                    return Err(e);
                }
                Ok(data) => data,
            };

            match self.bincode_options.deserialize::<DedupIndex>(&data) {
                Err(err) => {
                    event!(
                        Level::WARN,
                        ?key,
                        ?err,
                        "Failed to deserialize index in dedup store",
                    );
                    // We return the equivalent of NotFound here so the client is happy.
                    return Ok(None);
                }
                Ok(v) => v,
            }
        };

        let mut digests: Vec<_> = Vec::with_capacity(index_entries.entries.len() * 2);
        for index_entry in index_entries.entries.into_iter() {
            match index_entry {
                DedupEntry::Base(base_entry) => digests
                    .push(DigestInfo::new(base_entry.packed_hash, base_entry.size_bytes).into()),
                DedupEntry::Delta(base_entry, delta_entry, _) => {
                    digests.push(
                        DigestInfo::new(base_entry.packed_hash, base_entry.size_bytes).into(),
                    );
                    digests.push(
                        DigestInfo::new(delta_entry.packed_hash, delta_entry.size_bytes).into(),
                    );
                }
            }
        }
        let mut sum = 0;
        for size in self.content_store.has_many(&digests).await? {
            let Some(size) = size else {
                // A part is missing so return None meaning not-found.
                // This will abort all in-flight queries related to this request.
                return Ok(None);
            };
            sum += size;
        }
        Ok(Some(sum))
    }
}

#[async_trait]
impl StoreDriver for DedupStore {
    async fn has_with_results(
        self: Pin<&Self>,
        digests: &[StoreKey<'_>],
        results: &mut [Option<usize>],
    ) -> Result<(), Error> {
        digests
            .iter()
            .zip(results.iter_mut())
            .map(|(key, result)| async move {
                match self.has(key.borrow()).await {
                    Ok(maybe_size) => {
                        *result = maybe_size;
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            })
            .collect::<FuturesOrdered<_>>()
            .try_collect()
            .await?;
        Ok(())
    }

    async fn update(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        reader: DropCloserReadHalf,
        _size_info: UploadSizeInfo,
    ) -> Result<(), Error> {
        let mut bytes_reader = StreamReader::new(reader);
        let frame_reader = FramedRead::new(&mut bytes_reader, self.fast_cdc_decoder.clone());
        let index_entries = frame_reader
            .map(|r| r.err_tip(|| "Failed to decode frame from fast_cdc"))
            .map_ok(|frame| async move {
                let hash = blake3::hash(&frame[..]).into();
                let index_entry = DigestInfo::new(hash, frame.len() as i64);
                if self
                    .content_store
                    .has(index_entry)
                    .await
                    .err_tip(|| "Failed to call .has() in DedupStore::update()")?
                    .is_some()
                {
                    // If our store has this digest, we don't need to upload it.
                    return Result::<_, Error>::Ok(DedupEntry::Base(index_entry));
                }
                let tlsh_key = tlsh::build_key(&frame[..]);
                event!(Level::INFO, ?index_entry, ?tlsh_key,);
                if let Some(key) = tlsh_key.as_ref() {
                    let mut best_keys = vec![];
                    let mut best_dist = 36 * 414 + 1;

                    let (lower, upper) = tlsh::build_range(key);
                    let range =
                        StoreKey::new_str(lower.as_str())..=StoreKey::new_str(upper.as_str());
                    self.lsh_store
                        .list(range, |candidate| {
                            let dist = tlsh::code_distance_hex(
                                &key.as_bytes()[25..],
                                &candidate.as_str().as_bytes()[25..],
                            );
                            if dist < best_dist {
                                best_dist = dist;
                                best_keys.push(candidate.borrow().into_owned())
                            }
                            true
                        })
                        .await
                        .unwrap();

                    if let Some(best_key) = best_keys.last() {
                        event!(Level::INFO, ?key, ?best_key, ?best_dist);
                        let data = self
                            .lsh_store
                            .get_part_unchunked(best_key.to_owned(), 0, None)
                            .await
                            .err_tip(|| "Failed to read LSH store in dedup store")?;
                        let base_entry = self
                            .bincode_options
                            .deserialize::<DigestInfo>(&data)
                            .map_err(|e| {
                                make_err!(
                                    Code::Internal,
                                    "Failed to deserialize LSH entry in dedup_store : {:?}",
                                    e
                                )
                            })?;

                        if let Ok(data) = self
                            .content_store
                            .get_part_unchunked(StoreKey::Digest(base_entry), 0, None)
                            .await
                        {
                            let mut delta = Vec::with_capacity(frame.len());
                            qbsdiff::Bsdiff::new(&data[..], &frame[..])
                                .compare(std::io::Cursor::new(&mut delta))
                                .unwrap();
                            let hash = blake3::hash(&delta[..]).into();
                            let delta_entry = DigestInfo::new(hash, delta.len() as i64);
                            self.content_store
                                .update_oneshot(delta_entry, delta.into())
                                .await
                                .err_tip(|| "Failed to update content store in dedup_store")?;
                            event!(Level::INFO, ?index_entry, ?base_entry, ?delta_entry);
                            return Result::<_, Error>::Ok(DedupEntry::Delta(
                                base_entry,
                                delta_entry,
                                index_entry.size_bytes,
                            ));
                        }
                    }

                    let serialized_entry =
                        self.bincode_options.serialize(&index_entry).map_err(|e| {
                            make_err!(
                                Code::Internal,
                                "Failed to serialize LSH entry in dedup_store : {:?}",
                                e
                            )
                        })?;
                    self.lsh_store
                        .update_oneshot(StoreKey::new_str(key.as_str()), serialized_entry.into())
                        .await
                        .err_tip(|| "Failed to update LSH store in dedup_store")?;
                }
                self.content_store
                    .update_oneshot(index_entry, frame)
                    .await
                    .err_tip(|| "Failed to update content store in dedup_store")?;
                Ok(DedupEntry::Base(index_entry))
            })
            .try_buffered(self.max_concurrent_fetch_per_get)
            .try_collect()
            .await?;

        let serialized_index = self
            .bincode_options
            .serialize(&DedupIndex {
                entries: index_entries,
            })
            .map_err(|e| {
                make_err!(
                    Code::Internal,
                    "Failed to serialize index in dedup_store : {:?}",
                    e
                )
            })?;

        self.index_store
            .update_oneshot(key, serialized_index.into())
            .await
            .err_tip(|| "Failed to insert our index entry to index_store in dedup_store")?;

        Ok(())
    }

    async fn get_part(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        writer: &mut DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error> {
        // Special case for if a client tries to read zero bytes.
        if length == Some(0) {
            writer
                .send_eof()
                .err_tip(|| "Failed to write EOF out from get_part dedup")?;
            return Ok(());
        }
        // First we need to download the index that contains where the individual parts actually
        // can be fetched from.
        let index_entries = {
            let data = self
                .index_store
                .get_part_unchunked(key, 0, None)
                .await
                .err_tip(|| "Failed to read index store in dedup store")?;

            self.bincode_options
                .deserialize::<DedupIndex>(&data)
                .map_err(|e| {
                    make_err!(
                        Code::Internal,
                        "Failed to deserialize index in dedup_store::get_part : {:?}",
                        e
                    )
                })?
        };

        let mut start_byte_in_stream: usize = 0;
        let entries = {
            if offset == 0 && length.is_none() {
                index_entries.entries
            } else {
                let mut current_entries_sum = 0;
                let mut entries = Vec::with_capacity(index_entries.entries.len());
                for entry in index_entries.entries {
                    let first_byte = current_entries_sum;
                    let entry_size = usize::try_from(match entry {
                        DedupEntry::Base(base_entry) => base_entry.size_bytes,
                        DedupEntry::Delta(_, _, size_bytes) => size_bytes,
                    })
                    .err_tip(|| "Failed to convert to usize in DedupStore")?;
                    current_entries_sum += entry_size;
                    // Filter any items who's end byte is before the first requested byte.
                    if current_entries_sum <= offset {
                        start_byte_in_stream = current_entries_sum;
                        continue;
                    }
                    // If we are not going to read any bytes past the length we are done.
                    if let Some(length) = length {
                        if first_byte >= offset + length {
                            break;
                        }
                    }
                    entries.push(entry);
                }
                entries
            }
        };

        // Second we we create a stream of futures for each chunk, but buffer/limit them so only
        // `max_concurrent_fetch_per_get` will be executed at a time.
        // The results will be streamed out in the same order they are in the entries table.
        // The results will execute in a "window-like" fashion, meaning that if we limit to
        // 5 requests at a time, and request 3 is stalled, request 1 & 2 can be output and
        // request 4 & 5 can be executing (or finished) while waiting for 3 to finish.
        // Note: We will buffer our data here up to:
        // `config.max_size * config.max_concurrent_fetch_per_get` per `get_part()` request.
        let mut entries_stream = stream::iter(entries)
            .map(move |index_entry| async move {
                match index_entry {
                    DedupEntry::Base(base_entry) => {
                        let data = self
                            .content_store
                            .get_part_unchunked(base_entry, 0, None)
                            .await
                            .err_tip(|| "Failed to get_part in content_store in dedup_store")?;

                        Result::<_, Error>::Ok(data)
                    }
                    DedupEntry::Delta(base_entry, delta_entry, size_bytes) => {
                        let base = self
                            .content_store
                            .get_part_unchunked(base_entry, 0, None)
                            .await
                            .err_tip(|| "Failed to get_part in content_store in dedup_store")?;
                        let delta = self
                            .content_store
                            .get_part_unchunked(delta_entry, 0, None)
                            .await
                            .err_tip(|| "Failed to get_part in content_store in dedup_store")?;
                        let mut data = Vec::with_capacity(size_bytes.try_into()?);
                        let patcher = qbsdiff::Bspatch::new(delta.as_ref())?;
                        patcher.apply(base.as_ref(), std::io::Cursor::new(&mut data))?;
                        Result::<_, Error>::Ok(data.into())
                    }
                }
            })
            .buffered(self.max_concurrent_fetch_per_get);

        // Stream out the buffered data one at a time and write the data to our writer stream.
        // In the event any of these error, we will abort early and abandon all the rest of the
        // streamed data.
        // Note: Need to take special care to ensure we send the proper slice of data requested.
        let mut bytes_to_skip = offset - start_byte_in_stream;
        let mut bytes_to_send = length.unwrap_or(usize::MAX - offset);
        while let Some(result) = entries_stream.next().await {
            let mut data = result.err_tip(|| "Inner store iterator closed early in DedupStore")?;
            assert!(
                bytes_to_skip <= data.len(),
                "Formula above must be wrong, {} > {}",
                bytes_to_skip,
                data.len()
            );
            let end_pos = cmp::min(data.len(), bytes_to_send + bytes_to_skip);
            if bytes_to_skip != 0 || data.len() > bytes_to_send {
                data = data.slice(bytes_to_skip..end_pos);
            }
            writer
                .send(data)
                .await
                .err_tip(|| "Failed to write data to get_part dedup")?;
            bytes_to_send -= end_pos - bytes_to_skip;
            bytes_to_skip = 0;
        }

        // Finish our stream by writing our EOF and shutdown the stream.
        writer
            .send_eof()
            .err_tip(|| "Failed to write EOF out from get_part dedup")?;
        Ok(())
    }

    fn inner_store(&self, _digest: Option<StoreKey>) -> &dyn StoreDriver {
        self
    }

    fn as_any<'a>(&'a self) -> &'a (dyn std::any::Any + Sync + Send + 'static) {
        self
    }

    fn as_any_arc(self: Arc<Self>) -> Arc<dyn std::any::Any + Sync + Send + 'static> {
        self
    }
}

mod tlsh {
    use std::string::String;

    use tlsh2::TlshDefaultBuilder;

    pub fn build_key(frame: &[u8]) -> Option<String> {
        TlshDefaultBuilder::build_from(frame)
            .map(|t| t.hash())
            .map(|h| {
                let mut key = [0u8; 85];
                let p = build_prefix(&h[40..]);
                let _ = hex::encode_to_slice(&p[2..], &mut key[..12]);
                key[12] = b':';
                key[13..].copy_from_slice(&h[..]);
                String::from_utf8(key.into()).unwrap()
            })
    }

    pub fn build_range(key: &str) -> (String, String) {
        let mut lower = key.to_owned();
        lower.truncate(15);
        lower.push_str("0000000000000000000000000000000000000000000000000000000000000000000000");
        let mut upper = key.to_owned();
        upper.truncate(15);
        upper.push_str("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");
        (lower, upper)
    }

    #[inline(always)]
    const fn safe_unhex(b: u8) -> usize {
        ((b >> 6) * 9).wrapping_add(b) as usize & 15
    }

    fn build_prefix(tlsh: &[u8]) -> [u8; 8] {
        let code: &[u8; 32] = tlsh.try_into().unwrap();
        let mut hash = 0u64;
        for b in code {
            hash <<= 2;
            hash += HEX_QUADS_ARE_3[safe_unhex(*b)] as u64;
        }
        hash.to_be_bytes()
    }

    pub fn code_distance_hex(x: &[u8], y: &[u8]) -> u32 {
        x.iter()
            .zip(y.iter())
            .map(|(a, b)| {
                HEX_DIFF_VALUE[HEX_DIFF_INDEX[safe_unhex(*a)][safe_unhex(*b)] as usize] as u32
            })
            .sum()
    }

    const HEX_QUADS_ARE_3: [u8; 16] = [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 2, 2, 2, 3];

    const HEX_DIFF_VALUE: [u16; 10] = [0, 414, 828, 941, 1355, 1882, 1995, 2409, 2936, 3990];

    const HEX_DIFF_INDEX: [[u8; 16]; 16] = [
        [0, 1, 3, 6, 1, 2, 4, 7, 3, 4, 5, 8, 6, 7, 8, 9],
        [1, 0, 1, 3, 2, 1, 2, 4, 4, 3, 4, 5, 7, 6, 7, 8],
        [3, 1, 0, 1, 4, 2, 1, 2, 5, 4, 3, 4, 8, 7, 6, 7],
        [6, 3, 1, 0, 7, 4, 2, 1, 8, 5, 4, 3, 9, 8, 7, 6],
        [1, 2, 4, 7, 0, 1, 3, 6, 1, 2, 4, 7, 3, 4, 5, 8],
        [2, 1, 2, 4, 1, 0, 1, 3, 2, 1, 2, 4, 4, 3, 4, 5],
        [4, 2, 1, 2, 3, 1, 0, 1, 4, 2, 1, 2, 5, 4, 3, 4],
        [7, 4, 2, 1, 6, 3, 1, 0, 7, 4, 2, 1, 8, 5, 4, 3],
        [3, 4, 5, 8, 1, 2, 4, 7, 0, 1, 3, 6, 1, 2, 4, 7],
        [4, 3, 4, 5, 2, 1, 2, 4, 1, 0, 1, 3, 2, 1, 2, 4],
        [5, 4, 3, 4, 4, 2, 1, 2, 3, 1, 0, 1, 4, 2, 1, 2],
        [8, 5, 4, 3, 7, 4, 2, 1, 6, 3, 1, 0, 7, 4, 2, 1],
        [6, 7, 8, 9, 3, 4, 5, 8, 1, 2, 4, 7, 0, 1, 3, 6],
        [7, 6, 7, 8, 4, 3, 4, 5, 2, 1, 2, 4, 1, 0, 1, 3],
        [8, 7, 6, 7, 5, 4, 3, 4, 4, 2, 1, 2, 3, 1, 0, 1],
        [9, 8, 7, 6, 8, 5, 4, 3, 7, 4, 2, 1, 6, 3, 1, 0],
    ];
}

default_health_status_indicator!(DedupStore);
