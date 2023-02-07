// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use differential_dataflow::lattice::Lattice;
use differential_dataflow::trace::Description;
use mz_persist::location::{SeqNo, VersionedData};
use mz_persist_types::{Codec, Codec64};
use mz_proto::{IntoRustIfSome, ProtoType, RustType, TryFromProtoError};
use prost::Message;
use semver::Version;
use serde::{Deserialize, Serialize};
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use tracing::debug;
use uuid::Uuid;

use mz_ore::halt;

use crate::critical::CriticalReaderId;
use crate::error::{CodecMismatch, CodecMismatchT};
use crate::internal::metrics::Metrics;
use crate::internal::paths::{PartialBatchKey, PartialRollupKey};
use crate::internal::state::{
    CriticalReaderState, HandleDebugState, HollowBatch, HollowBatchPart, IdempotencyToken,
    LeasedReaderState, OpaqueState, ProtoCriticalReaderState, ProtoHandleDebugState,
    ProtoHollowBatch, ProtoHollowBatchPart, ProtoLeasedReaderState, ProtoStateDiff,
    ProtoStateField, ProtoStateFieldDiffType, ProtoStateFieldDiffs, ProtoStateRollup, ProtoTrace,
    ProtoU64Antichain, ProtoU64Description, ProtoWriterState, State, StateCollections, TypedState,
    WriterState,
};
use crate::internal::state_diff::{
    ProtoStateFieldDiff, StateDiff, StateFieldDiff, StateFieldValDiff,
};
use crate::internal::trace::Trace;
use crate::read::LeasedReaderId;
use crate::write::WriterEnrichedHollowBatch;
use crate::{PersistConfig, ShardId, WriterId};

pub(crate) fn parse_id(id_prefix: char, id_type: &str, encoded: &str) -> Result<[u8; 16], String> {
    let uuid_encoded = match encoded.strip_prefix(id_prefix) {
        Some(x) => x,
        None => return Err(format!("invalid {} {}: incorrect prefix", id_type, encoded)),
    };
    let uuid = Uuid::parse_str(uuid_encoded)
        .map_err(|err| format!("invalid {} {}: {}", id_type, encoded, err))?;
    Ok(*uuid.as_bytes())
}

// If persist gets some encoded ProtoState from the future (e.g. two versions of
// code are running simultaneously against the same shard), it might have a
// field that the current code doesn't know about. This would be silently
// discarded at proto decode time. Unknown Fields [1] are a tool we can use in
// the future to help deal with this, but in the short-term, it's best to keep
// the persist read-modify-CaS loop simple for as long as we can get away with
// it (i.e. until we have to offer the ability to do rollbacks).
//
// [1]: https://developers.google.com/protocol-buffers/docs/proto3#unknowns
//
// To detect the bad situation and disallow it, we tag every version of state
// written to consensus with the version of code used to encode it. Then at
// decode time, we're able to compare the current version against any we receive
// and assert as necessary.
//
// Initially we reject any version from the future (no forward compatibility,
// most conservative but easiest to reason about) but allow any from the past
// (permanent backward compatibility). If/when we support deploy rollbacks and
// rolling upgrades, we can adjust this assert as necessary to reflect the
// policy (e.g. by adding some window of X allowed versions of forward
// compatibility, computed by comparing semvers).
//
// We could do the same for blob data, but it shouldn't be necessary. Any blob
// data we read is going to be because we fetched it using a pointer stored in
// some persist state. If we can handle the state, we can handle the blobs it
// references, too.
fn check_applier_version(build_version: &Version, applier_version: &Version) {
    if build_version < applier_version {
        halt!(
            "{} received persist state from the future {}",
            build_version,
            applier_version,
        );
    }
}

impl RustType<String> for ShardId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match proto.parse() {
            Ok(x) => Ok(x),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for LeasedReaderId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match proto.parse() {
            Ok(x) => Ok(x),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for CriticalReaderId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match proto.parse() {
            Ok(x) => Ok(x),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for WriterId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match proto.parse() {
            Ok(x) => Ok(x),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for IdempotencyToken {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match proto.parse() {
            Ok(x) => Ok(x),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for PartialBatchKey {
    fn into_proto(&self) -> String {
        self.0.clone()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        Ok(PartialBatchKey(proto))
    }
}

impl RustType<String> for PartialRollupKey {
    fn into_proto(&self) -> String {
        self.0.clone()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        Ok(PartialRollupKey(proto))
    }
}

impl<T: Timestamp + Lattice + Codec64> StateDiff<T> {
    pub fn encode<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut,
    {
        self.into_proto()
            .encode(buf)
            .expect("no required fields means no initialization errors");
    }

    pub fn decode(build_version: &Version, buf: &[u8]) -> Self {
        let proto = ProtoStateDiff::decode(buf)
            // We received a State that we couldn't decode. This could happen if
            // persist messes up backward/forward compatibility, if the durable
            // data was corrupted, or if operations messes up deployment. In any
            // case, fail loudly.
            .expect("internal error: invalid encoded state");
        let diff = Self::from_proto(proto).expect("internal error: invalid encoded state");
        check_applier_version(build_version, &diff.applier_version);
        diff
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoStateDiff> for StateDiff<T> {
    fn into_proto(&self) -> ProtoStateDiff {
        // Deconstruct self so we get a compile failure if new fields are added.
        let StateDiff {
            applier_version,
            seqno_from,
            seqno_to,
            walltime_ms,
            latest_rollup_key,
            rollups,
            hostname,
            last_gc_req,
            leased_readers,
            critical_readers,
            writers,
            since,
            spine,
        } = self;

        let mut field_diffs = ProtoStateFieldDiffs::default();
        field_diffs_into_proto(
            ProtoStateField::Hostname,
            hostname,
            &mut field_diffs,
            |()| Vec::new(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::LastGcReq,
            last_gc_req,
            &mut field_diffs,
            |()| Vec::new(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::Rollups,
            rollups,
            &mut field_diffs,
            |k| k.into_proto().encode_to_vec(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::LeasedReaders,
            leased_readers,
            &mut field_diffs,
            |k| k.into_proto().encode_to_vec(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::CriticalReaders,
            critical_readers,
            &mut field_diffs,
            |k| k.into_proto().encode_to_vec(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::Writers,
            writers,
            &mut field_diffs,
            |k| k.into_proto().encode_to_vec(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::Since,
            since,
            &mut field_diffs,
            |()| Vec::new(),
            |v| v.into_proto().encode_to_vec(),
        );
        field_diffs_into_proto(
            ProtoStateField::Spine,
            spine,
            &mut field_diffs,
            |k| k.into_proto().encode_to_vec(),
            |()| Vec::new(),
        );
        debug_assert_eq!(field_diffs.validate(), Ok(()));
        ProtoStateDiff {
            applier_version: applier_version.to_string(),
            seqno_from: seqno_from.into_proto(),
            seqno_to: seqno_to.into_proto(),
            walltime_ms: walltime_ms.into_proto(),
            latest_rollup_key: latest_rollup_key.into_proto(),
            field_diffs: Some(field_diffs),
        }
    }

    fn from_proto(proto: ProtoStateDiff) -> Result<Self, TryFromProtoError> {
        let applier_version = if proto.applier_version.is_empty() {
            // Backward compatibility with versions of ProtoState before we set
            // this field: if it's missing (empty), assume an infinitely old
            // version.
            semver::Version::new(0, 0, 0)
        } else {
            semver::Version::parse(&proto.applier_version).map_err(|err| {
                TryFromProtoError::InvalidSemverVersion(format!(
                    "invalid applier_version {}: {}",
                    proto.applier_version, err
                ))
            })?
        };
        let mut state_diff = StateDiff::new(
            applier_version,
            proto.seqno_from.into_rust()?,
            proto.seqno_to.into_rust()?,
            proto.walltime_ms,
            proto.latest_rollup_key.into_rust()?,
        );
        if let Some(field_diffs) = proto.field_diffs {
            debug_assert_eq!(field_diffs.validate(), Ok(()));
            for field_diff in field_diffs.iter() {
                let (field, diff) = field_diff?;
                match field {
                    ProtoStateField::Hostname => field_diff_into_rust::<(), String, _, _, _, _>(
                        diff,
                        &mut state_diff.hostname,
                        |()| Ok(()),
                        |v| v.into_rust(),
                    )?,
                    ProtoStateField::LastGcReq => field_diff_into_rust::<(), u64, _, _, _, _>(
                        diff,
                        &mut state_diff.last_gc_req,
                        |()| Ok(()),
                        |v| v.into_rust(),
                    )?,
                    ProtoStateField::Rollups => field_diff_into_rust::<u64, String, _, _, _, _>(
                        diff,
                        &mut state_diff.rollups,
                        |k| k.into_rust(),
                        |v| v.into_rust(),
                    )?,
                    ProtoStateField::LeasedReaders => {
                        field_diff_into_rust::<String, ProtoLeasedReaderState, _, _, _, _>(
                            diff,
                            &mut state_diff.leased_readers,
                            |k| k.into_rust(),
                            |v| v.into_rust(),
                        )?
                    }
                    ProtoStateField::CriticalReaders => {
                        field_diff_into_rust::<String, ProtoCriticalReaderState, _, _, _, _>(
                            diff,
                            &mut state_diff.critical_readers,
                            |k| k.into_rust(),
                            |v| v.into_rust(),
                        )?
                    }
                    ProtoStateField::Writers => {
                        field_diff_into_rust::<String, ProtoWriterState, _, _, _, _>(
                            diff,
                            &mut state_diff.writers,
                            |k| k.into_rust(),
                            |v| v.into_rust(),
                        )?
                    }
                    ProtoStateField::Since => {
                        field_diff_into_rust::<(), ProtoU64Antichain, _, _, _, _>(
                            diff,
                            &mut state_diff.since,
                            |()| Ok(()),
                            |v| v.into_rust(),
                        )?
                    }
                    ProtoStateField::Spine => {
                        field_diff_into_rust::<ProtoHollowBatch, (), _, _, _, _>(
                            diff,
                            &mut state_diff.spine,
                            |k| k.into_rust(),
                            |()| Ok(()),
                        )?
                    }
                }
            }
        }
        Ok(state_diff)
    }
}

fn field_diffs_into_proto<K, V, KFn, VFn>(
    field: ProtoStateField,
    diffs: &[StateFieldDiff<K, V>],
    proto: &mut ProtoStateFieldDiffs,
    k_fn: KFn,
    v_fn: VFn,
) where
    KFn: Fn(&K) -> Vec<u8>,
    VFn: Fn(&V) -> Vec<u8>,
{
    for diff in diffs.iter() {
        field_diff_into_proto(field, diff, proto, &k_fn, &v_fn);
    }
}

fn field_diff_into_proto<K, V, KFn, VFn>(
    field: ProtoStateField,
    diff: &StateFieldDiff<K, V>,
    proto: &mut ProtoStateFieldDiffs,
    k_fn: KFn,
    v_fn: VFn,
) where
    KFn: Fn(&K) -> Vec<u8>,
    VFn: Fn(&V) -> Vec<u8>,
{
    proto.fields.push(i32::from(field));
    proto.push_data(k_fn(&diff.key));
    match &diff.val {
        StateFieldValDiff::Insert(to) => {
            proto
                .diff_types
                .push(i32::from(ProtoStateFieldDiffType::Insert));
            proto.push_data(v_fn(to));
        }
        StateFieldValDiff::Update(from, to) => {
            proto
                .diff_types
                .push(i32::from(ProtoStateFieldDiffType::Update));
            proto.push_data(v_fn(from));
            proto.push_data(v_fn(to));
        }
        StateFieldValDiff::Delete(from) => {
            proto
                .diff_types
                .push(i32::from(ProtoStateFieldDiffType::Delete));
            proto.push_data(v_fn(from));
        }
    };
}

fn field_diff_into_rust<KP, VP, K, V, KFn, VFn>(
    proto: ProtoStateFieldDiff<'_>,
    diffs: &mut Vec<StateFieldDiff<K, V>>,
    k_fn: KFn,
    v_fn: VFn,
) -> Result<(), TryFromProtoError>
where
    KP: prost::Message + Default,
    VP: prost::Message + Default,
    KFn: Fn(KP) -> Result<K, TryFromProtoError>,
    VFn: Fn(VP) -> Result<V, TryFromProtoError>,
{
    let val = match proto.diff_type {
        ProtoStateFieldDiffType::Insert => {
            let to = VP::decode(proto.to)
                .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
            StateFieldValDiff::Insert(v_fn(to)?)
        }
        ProtoStateFieldDiffType::Update => {
            let from = VP::decode(proto.from)
                .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
            let to = VP::decode(proto.to)
                .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;

            StateFieldValDiff::Update(v_fn(from)?, v_fn(to)?)
        }
        ProtoStateFieldDiffType::Delete => {
            let from = VP::decode(proto.from)
                .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
            StateFieldValDiff::Delete(v_fn(from)?)
        }
    };
    let key = KP::decode(proto.key)
        .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
    diffs.push(StateFieldDiff {
        key: k_fn(key)?,
        val,
    });
    Ok(())
}

impl<K, V, T, D> TypedState<K, V, T, D>
where
    K: Codec,
    V: Codec,
    T: Timestamp + Lattice + Codec64,
    D: Codec64,
{
    pub fn encode<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut,
    {
        self.into_proto()
            .encode(buf)
            .expect("no required fields means no initialization errors");
    }

    pub(crate) fn into_proto(&self) -> ProtoStateRollup {
        self.state
            .into_proto(K::codec_name(), V::codec_name(), D::codec_name())
    }
}

impl<T> State<T>
where
    T: Timestamp + Lattice + Codec64,
{
    pub(crate) fn into_proto(
        &self,
        key_codec: String,
        val_codec: String,
        diff_codec: String,
    ) -> ProtoStateRollup {
        ProtoStateRollup {
            applier_version: self.applier_version.to_string(),
            shard_id: self.shard_id.into_proto(),
            seqno: self.seqno.into_proto(),
            walltime_ms: self.walltime_ms.into_proto(),
            hostname: self.hostname.into_proto(),
            key_codec,
            val_codec,
            ts_codec: T::codec_name(),
            diff_codec,
            last_gc_req: self.collections.last_gc_req.into_proto(),
            rollups: self
                .collections
                .rollups
                .iter()
                .map(|(seqno, key)| (seqno.into_proto(), key.into_proto()))
                .collect(),
            leased_readers: self
                .collections
                .leased_readers
                .iter()
                .map(|(id, state)| (id.into_proto(), state.into_proto()))
                .collect(),
            critical_readers: self
                .collections
                .critical_readers
                .iter()
                .map(|(id, state)| (id.into_proto(), state.into_proto()))
                .collect(),
            writers: self
                .collections
                .writers
                .iter()
                .map(|(id, state)| (id.into_proto(), state.into_proto()))
                .collect(),
            trace: Some(self.collections.trace.into_proto()),
        }
    }
}

/// A decoded version of [ProtoStateRollup] for which we have not yet checked
/// that codecs match the ones in durable state.
#[derive(Debug)]
pub struct UntypedState<T> {
    pub(crate) key_codec: String,
    pub(crate) val_codec: String,
    pub(crate) ts_codec: String,
    pub(crate) diff_codec: String,

    // Important! This T has not been validated, so we can't expose anything in
    // State that references T until one of the check methods have been called.
    // Any field on State that doesn't reference T is fair game.
    state: State<T>,
}

impl<T: Timestamp + Lattice + Codec64> UntypedState<T> {
    pub fn seqno(&self) -> SeqNo {
        self.state.seqno
    }

    pub fn rollups(&self) -> &BTreeMap<SeqNo, PartialRollupKey> {
        &self.state.collections.rollups
    }

    pub fn apply_encoded_diffs<'a, I: IntoIterator<Item = &'a VersionedData>>(
        &mut self,
        cfg: &PersistConfig,
        metrics: &Metrics,
        diffs: I,
    ) {
        // The apply_encoded_diffs might panic if T is not correct. Making this
        // a silent no-op is far too subtle for my taste, but it's not clear
        // what else we could do instead.
        if T::codec_name() != self.ts_codec {
            return;
        }
        self.state.apply_encoded_diffs(cfg, metrics, diffs);
    }

    pub fn check_codecs<K: Codec, V: Codec, D: Codec64>(
        self,
        shard_id: &ShardId,
    ) -> Result<TypedState<K, V, T, D>, Box<CodecMismatch>> {
        // Also defensively check that the shard_id on the state we fetched
        // matches the shard_id we were trying to fetch.
        assert_eq!(shard_id, &self.state.shard_id);
        if K::codec_name() != self.key_codec
            || V::codec_name() != self.val_codec
            || T::codec_name() != self.ts_codec
            || D::codec_name() != self.diff_codec
        {
            return Err(Box::new(CodecMismatch {
                requested: (
                    K::codec_name(),
                    V::codec_name(),
                    T::codec_name(),
                    D::codec_name(),
                    None,
                ),
                actual: (
                    self.key_codec,
                    self.val_codec,
                    self.ts_codec,
                    self.diff_codec,
                    None,
                ),
            }));
        }
        Ok(TypedState {
            state: self.state,
            _phantom: PhantomData,
        })
    }

    pub(crate) fn check_ts_codec(self, shard_id: &ShardId) -> Result<State<T>, CodecMismatchT> {
        // Also defensively check that the shard_id on the state we fetched
        // matches the shard_id we were trying to fetch.
        assert_eq!(shard_id, &self.state.shard_id);
        if T::codec_name() != self.ts_codec {
            return Err(CodecMismatchT {
                requested: T::codec_name(),
                actual: self.ts_codec,
            });
        }
        Ok(self.state)
    }

    pub fn decode(build_version: &Version, buf: &[u8]) -> Self {
        let proto = ProtoStateRollup::decode(buf)
            // We received a State that we couldn't decode. This could happen if
            // persist messes up backward/forward compatibility, if the durable
            // data was corrupted, or if operations messes up deployment. In any
            // case, fail loudly.
            .expect("internal error: invalid encoded state");
        let state = Self::from_proto(proto).expect("internal error: invalid encoded state");
        check_applier_version(build_version, &state.state.applier_version);
        state
    }
}

impl<T: Timestamp + Lattice + Codec64> RustType<ProtoStateRollup> for UntypedState<T> {
    fn into_proto(&self) -> ProtoStateRollup {
        self.state.into_proto(
            self.key_codec.clone(),
            self.val_codec.clone(),
            self.diff_codec.clone(),
        )
    }

    fn from_proto(x: ProtoStateRollup) -> Result<Self, TryFromProtoError> {
        let applier_version = if x.applier_version.is_empty() {
            // Backward compatibility with versions of ProtoState before we set
            // this field: if it's missing (empty), assume an infinitely old
            // version.
            semver::Version::new(0, 0, 0)
        } else {
            semver::Version::parse(&x.applier_version).map_err(|err| {
                TryFromProtoError::InvalidSemverVersion(format!(
                    "invalid applier_version {}: {}",
                    x.applier_version, err
                ))
            })?
        };

        let mut rollups = BTreeMap::new();
        for (seqno, key) in x.rollups {
            rollups.insert(seqno.into_rust()?, key.into_rust()?);
        }
        let mut leased_readers = BTreeMap::new();
        for (id, state) in x.leased_readers {
            leased_readers.insert(id.into_rust()?, state.into_rust()?);
        }
        let mut critical_readers = BTreeMap::new();
        for (id, state) in x.critical_readers {
            critical_readers.insert(id.into_rust()?, state.into_rust()?);
        }
        let mut writers = BTreeMap::new();
        for (id, state) in x.writers {
            writers.insert(id.into_rust()?, state.into_rust()?);
        }
        let collections = StateCollections {
            rollups,
            last_gc_req: x.last_gc_req.into_rust()?,
            leased_readers,
            critical_readers,
            writers,
            trace: x.trace.into_rust_if_some("trace")?,
        };
        let state = State {
            applier_version,
            shard_id: x.shard_id.into_rust()?,
            seqno: x.seqno.into_rust()?,
            walltime_ms: x.walltime_ms,
            hostname: x.hostname,
            collections,
        };
        Ok(UntypedState {
            state,
            key_codec: x.key_codec.into_rust()?,
            val_codec: x.val_codec.into_rust()?,
            ts_codec: x.ts_codec.into_rust()?,
            diff_codec: x.diff_codec.into_rust()?,
        })
    }
}

impl<T: Timestamp + Lattice + Codec64> RustType<ProtoTrace> for Trace<T> {
    fn into_proto(&self) -> ProtoTrace {
        let mut spine = Vec::new();
        self.map_batches(|b| {
            spine.push(b.into_proto());
        });
        ProtoTrace {
            since: Some(self.since().into_proto()),
            spine,
        }
    }

    fn from_proto(proto: ProtoTrace) -> Result<Self, TryFromProtoError> {
        let mut ret = Trace::default();
        ret.downgrade_since(&proto.since.into_rust_if_some("since")?);
        let mut batches_pushed = 0;
        for batch in proto.spine.into_iter() {
            let batch: HollowBatch<T> = batch.into_rust()?;
            if PartialOrder::less_than(ret.since(), batch.desc.since()) {
                return Err(TryFromProtoError::InvalidPersistState(format!(
                    "invalid ProtoTrace: the spine's since {:?} was less than a batch's since {:?}",
                    ret.since(),
                    batch.desc.since()
                )));
            }
            // We could perhaps more directly serialize and rehydrate the
            // internals of the Spine, but this is nice because it insulates
            // us against changes in the Spine logic. The current logic has
            // turned out to be relatively expensive in practice, but as we
            // tune things (especially when we add inc state) the rate of
            // this deserialization should go down. Revisit as necessary.
            //
            // Ignore merge_reqs because whichever process generated this diff is
            // assigned the work.
            let _merge_reqs = ret.push_batch(batch);

            batches_pushed += 1;
            if batches_pushed % 1000 == 0 {
                let mut batch_count = 0;
                ret.map_batches(|_| batch_count += 1);
                debug!("Decoded and pushed {batches_pushed} batches; trace size {batch_count}");
            }
        }
        Ok(ret)
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoLeasedReaderState> for LeasedReaderState<T> {
    fn into_proto(&self) -> ProtoLeasedReaderState {
        ProtoLeasedReaderState {
            seqno: self.seqno.into_proto(),
            since: Some(self.since.into_proto()),
            last_heartbeat_timestamp_ms: self.last_heartbeat_timestamp_ms.into_proto(),
            lease_duration_ms: self.lease_duration_ms.into_proto(),
            debug: Some(self.debug.into_proto()),
        }
    }

    fn from_proto(proto: ProtoLeasedReaderState) -> Result<Self, TryFromProtoError> {
        let mut lease_duration_ms = proto.lease_duration_ms.into_rust()?;
        // MIGRATION: If the lease_duration_ms is empty, then the proto field
        // was missing and we need to fill in a default. This would ideally be
        // based on the actual value in PersistConfig, but it's only here for a
        // short time and this is way easier.
        if lease_duration_ms == 0 {
            lease_duration_ms =
                u64::try_from(PersistConfig::DEFAULT_READ_LEASE_DURATION.as_millis())
                    .expect("lease duration as millis should fit within u64");
        }
        // MIGRATION: If debug is empty, then the proto field was missing and we
        // need to fill in a default.
        let debug = proto.debug.unwrap_or_default().into_rust()?;
        Ok(LeasedReaderState {
            seqno: proto.seqno.into_rust()?,
            since: proto
                .since
                .into_rust_if_some("ProtoLeasedReaderState::since")?,
            last_heartbeat_timestamp_ms: proto.last_heartbeat_timestamp_ms.into_rust()?,
            lease_duration_ms,
            debug,
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoCriticalReaderState> for CriticalReaderState<T> {
    fn into_proto(&self) -> ProtoCriticalReaderState {
        ProtoCriticalReaderState {
            since: Some(self.since.into_proto()),
            opaque: i64::from_le_bytes(self.opaque.0),
            opaque_codec: self.opaque_codec.clone(),
            debug: Some(self.debug.into_proto()),
        }
    }

    fn from_proto(proto: ProtoCriticalReaderState) -> Result<Self, TryFromProtoError> {
        // MIGRATION: If debug is empty, then the proto field was missing and we
        // need to fill in a default.
        let debug = proto.debug.unwrap_or_default().into_rust()?;
        Ok(CriticalReaderState {
            since: proto
                .since
                .into_rust_if_some("ProtoCriticalReaderState::since")?,
            opaque: OpaqueState(i64::to_le_bytes(proto.opaque)),
            opaque_codec: proto.opaque_codec,
            debug,
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoWriterState> for WriterState<T> {
    fn into_proto(&self) -> ProtoWriterState {
        ProtoWriterState {
            last_heartbeat_timestamp_ms: self.last_heartbeat_timestamp_ms.into_proto(),
            lease_duration_ms: self.lease_duration_ms.into_proto(),
            most_recent_write_token: self.most_recent_write_token.into_proto(),
            most_recent_write_upper: Some(self.most_recent_write_upper.into_proto()),
            debug: Some(self.debug.into_proto()),
        }
    }

    fn from_proto(proto: ProtoWriterState) -> Result<Self, TryFromProtoError> {
        // MIGRATION: We didn't originally have most_recent_write_token and
        // most_recent_write_upper. Pick values that aren't going to
        // accidentally match ones in incoming writes and confuse things. We
        // could instead use Option on WriterState but this keeps the backward
        // compatibility logic confined to one place.
        let most_recent_write_token = if proto.most_recent_write_token.is_empty() {
            IdempotencyToken::SENTINEL
        } else {
            proto.most_recent_write_token.into_rust()?
        };
        let most_recent_write_upper = match proto.most_recent_write_upper {
            Some(x) => x.into_rust()?,
            None => Antichain::from_elem(T::minimum()),
        };
        // MIGRATION: If debug is empty, then the proto field was missing and we
        // need to fill in a default.
        let debug = proto.debug.unwrap_or_default().into_rust()?;
        Ok(WriterState {
            last_heartbeat_timestamp_ms: proto.last_heartbeat_timestamp_ms.into_rust()?,
            lease_duration_ms: proto.lease_duration_ms.into_rust()?,
            most_recent_write_token,
            most_recent_write_upper,
            debug,
        })
    }
}

impl RustType<ProtoHandleDebugState> for HandleDebugState {
    fn into_proto(&self) -> ProtoHandleDebugState {
        ProtoHandleDebugState {
            hostname: self.hostname.into_proto(),
            purpose: self.purpose.into_proto(),
        }
    }

    fn from_proto(proto: ProtoHandleDebugState) -> Result<Self, TryFromProtoError> {
        Ok(HandleDebugState {
            hostname: proto.hostname,
            purpose: proto.purpose,
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoHollowBatch> for HollowBatch<T> {
    fn into_proto(&self) -> ProtoHollowBatch {
        ProtoHollowBatch {
            desc: Some(self.desc.into_proto()),
            parts: self.parts.into_proto(),
            len: self.len.into_proto(),
            runs: self.runs.into_proto(),
            deprecated_keys: vec![],
        }
    }

    fn from_proto(proto: ProtoHollowBatch) -> Result<Self, TryFromProtoError> {
        let mut parts: Vec<HollowBatchPart> = proto.parts.into_rust()?;
        // MIGRATION: We used to just have the keys instead of a more structured
        // part.
        parts.extend(
            proto
                .deprecated_keys
                .into_iter()
                .map(|key| HollowBatchPart {
                    key: PartialBatchKey(key),
                    encoded_size_bytes: 0,
                }),
        );
        Ok(HollowBatch {
            desc: proto.desc.into_rust_if_some("desc")?,
            parts,
            len: proto.len.into_rust()?,
            runs: proto.runs.into_rust()?,
        })
    }
}

impl RustType<ProtoHollowBatchPart> for HollowBatchPart {
    fn into_proto(&self) -> ProtoHollowBatchPart {
        ProtoHollowBatchPart {
            key: self.key.into_proto(),
            encoded_size_bytes: self.encoded_size_bytes.into_proto(),
        }
    }

    fn from_proto(proto: ProtoHollowBatchPart) -> Result<Self, TryFromProtoError> {
        Ok(HollowBatchPart {
            key: proto.key.into_rust()?,
            encoded_size_bytes: proto.encoded_size_bytes.into_rust()?,
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoU64Description> for Description<T> {
    fn into_proto(&self) -> ProtoU64Description {
        ProtoU64Description {
            lower: Some(self.lower().into_proto()),
            upper: Some(self.upper().into_proto()),
            since: Some(self.since().into_proto()),
        }
    }

    fn from_proto(proto: ProtoU64Description) -> Result<Self, TryFromProtoError> {
        Ok(Description::new(
            proto.lower.into_rust_if_some("lower")?,
            proto.upper.into_rust_if_some("upper")?,
            proto.since.into_rust_if_some("since")?,
        ))
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoU64Antichain> for Antichain<T> {
    fn into_proto(&self) -> ProtoU64Antichain {
        ProtoU64Antichain {
            elements: self
                .elements()
                .iter()
                .map(|x| i64::from_le_bytes(T::encode(x)))
                .collect(),
        }
    }

    fn from_proto(proto: ProtoU64Antichain) -> Result<Self, TryFromProtoError> {
        let elements = proto
            .elements
            .iter()
            .map(|x| T::decode(x.to_le_bytes()))
            .collect::<Vec<_>>();
        Ok(Antichain::from(elements))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SerdeWriterEnrichedHollowBatch {
    pub(crate) shard_id: ShardId,
    pub(crate) batch: Vec<u8>,
}

impl<T: Timestamp + Codec64> From<WriterEnrichedHollowBatch<T>> for SerdeWriterEnrichedHollowBatch {
    fn from(x: WriterEnrichedHollowBatch<T>) -> Self {
        SerdeWriterEnrichedHollowBatch {
            shard_id: x.shard_id,
            batch: x.batch.into_proto().encode_to_vec(),
        }
    }
}

impl<T: Timestamp + Codec64> From<SerdeWriterEnrichedHollowBatch> for WriterEnrichedHollowBatch<T> {
    fn from(x: SerdeWriterEnrichedHollowBatch) -> Self {
        let proto_batch = ProtoHollowBatch::decode(x.batch.as_slice())
            .expect("internal error: could not decode WriterEnrichedHollowBatch");
        let batch = proto_batch
            .into_rust()
            .expect("internal error: could not decode WriterEnrichedHollowBatch");
        WriterEnrichedHollowBatch {
            shard_id: x.shard_id,
            batch,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use mz_persist::location::SeqNo;

    use crate::internal::paths::PartialRollupKey;
    use crate::internal::state::HandleDebugState;
    use crate::internal::state_diff::StateDiff;
    use crate::ShardId;

    use super::*;

    #[test]
    fn applier_version_state() {
        let v1 = semver::Version::new(1, 0, 0);
        let v2 = semver::Version::new(2, 0, 0);
        let v3 = semver::Version::new(3, 0, 0);

        // Code version v2 evaluates and writes out some State.
        let shard_id = ShardId::new();
        let state = TypedState::<(), (), u64, i64>::new(v2.clone(), shard_id, "".to_owned(), 0);
        let mut buf = Vec::new();
        state.encode(&mut buf);

        // We can read it back using persist code v2 and v3.
        assert_eq!(
            UntypedState::<u64>::decode(&v2, &buf)
                .check_codecs(&shard_id)
                .as_ref(),
            Ok(&state)
        );
        assert_eq!(
            UntypedState::<u64>::decode(&v3, &buf)
                .check_codecs(&shard_id)
                .as_ref(),
            Ok(&state)
        );

        // But we can't read it back using v1 because v1 might corrupt it by
        // losing or misinterpreting something written out by a future version
        // of code.
        mz_ore::process::PANIC_ON_HALT.store(true, Ordering::SeqCst);
        let v1_res = mz_ore::panic::catch_unwind(|| UntypedState::<u64>::decode(&v1, &buf));
        assert!(v1_res.is_err());
    }

    #[test]
    fn applier_version_state_diff() {
        let v1 = semver::Version::new(1, 0, 0);
        let v2 = semver::Version::new(2, 0, 0);
        let v3 = semver::Version::new(3, 0, 0);

        // Code version v2 evaluates and writes out some State.
        let diff = StateDiff::<u64>::new(
            v2.clone(),
            SeqNo(0),
            SeqNo(1),
            2,
            PartialRollupKey("rollup".into()),
        );
        let mut buf = Vec::new();
        diff.encode(&mut buf);

        // We can read it back using persist code v2 and v3.
        assert_eq!(StateDiff::decode(&v2, &buf), diff);
        assert_eq!(StateDiff::decode(&v3, &buf), diff);

        // But we can't read it back using v1 because v1 might corrupt it by
        // losing or misinterpreting something written out by a future version
        // of code.
        mz_ore::process::PANIC_ON_HALT.store(true, Ordering::SeqCst);
        let v1_res = mz_ore::panic::catch_unwind(|| StateDiff::<u64>::decode(&v1, &buf));
        assert!(v1_res.is_err());
    }

    #[test]
    fn hollow_batch_migration_keys() {
        let x = HollowBatch {
            desc: Description::new(
                Antichain::from_elem(1u64),
                Antichain::from_elem(2u64),
                Antichain::from_elem(3u64),
            ),
            len: 4,
            parts: vec![HollowBatchPart {
                key: PartialBatchKey("a".into()),
                encoded_size_bytes: 5,
            }],
            runs: vec![],
        };
        let mut old = x.into_proto();
        // Old ProtoHollowBatch had keys instead of parts.
        old.deprecated_keys = vec!["b".into()];
        // We don't expect to see a ProtoHollowBatch with keys _and_ parts, only
        // one or the other, but we have a defined output, so may as well test
        // it.
        let mut expected = x;
        // We fill in 0 for encoded_size_bytes when we migrate from keys. This
        // will violate bounded memory usage compaction during the transition
        // (short-term issue), but that's better than creating unnecessary runs
        // (longer-term issue).
        expected.parts.push(HollowBatchPart {
            key: PartialBatchKey("b".into()),
            encoded_size_bytes: 0,
        });
        assert_eq!(<HollowBatch<u64>>::from_proto(old).unwrap(), expected);
    }

    #[test]
    fn reader_state_migration_lease_duration() {
        let x = LeasedReaderState {
            seqno: SeqNo(1),
            since: Antichain::from_elem(2u64),
            last_heartbeat_timestamp_ms: 3,
            debug: HandleDebugState {
                hostname: "host".to_owned(),
                purpose: "purpose".to_owned(),
            },
            // Old ProtoReaderState had no lease_duration_ms field
            lease_duration_ms: 0,
        };
        let old = x.into_proto();
        let mut expected = x;
        // We fill in DEFAULT_READ_LEASE_DURATION for lease_duration_ms when we
        // migrate from unset.
        expected.lease_duration_ms =
            u64::try_from(PersistConfig::DEFAULT_READ_LEASE_DURATION.as_millis()).unwrap();
        assert_eq!(<LeasedReaderState<u64>>::from_proto(old).unwrap(), expected);
    }

    #[test]
    fn writer_state_migration_most_recent_write() {
        let proto = ProtoWriterState {
            last_heartbeat_timestamp_ms: 1,
            lease_duration_ms: 2,
            // Old ProtoWriterState had no most_recent_write_token or
            // most_recent_write_upper.
            most_recent_write_token: "".into(),
            most_recent_write_upper: None,
            debug: Some(ProtoHandleDebugState {
                hostname: "host".to_owned(),
                purpose: "purpose".to_owned(),
            }),
        };
        let expected = WriterState {
            last_heartbeat_timestamp_ms: proto.last_heartbeat_timestamp_ms,
            lease_duration_ms: proto.lease_duration_ms,
            most_recent_write_token: IdempotencyToken::SENTINEL,
            most_recent_write_upper: Antichain::from_elem(0),
            debug: HandleDebugState {
                hostname: "host".to_owned(),
                purpose: "purpose".to_owned(),
            },
        };
        assert_eq!(<WriterState<u64>>::from_proto(proto).unwrap(), expected);
    }
}
