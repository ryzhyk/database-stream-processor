use crate::{
    algebra::{DefaultSemigroup, GroupValue, HasOne, HasZero, IndexedZSet, MulByRef, ZRingValue},
    circuit::{
        operator_traits::{Operator, QuaternaryOperator},
        OwnershipPreference, Scope,
    },
    operator::{
        time_series::{
            radix_tree::{PartitionedRadixTreeReader, RadixTreeCursor},
            range::{Range, RangeCursor, Ranges, RelRange},
            OrdPartitionedIndexedZSet, PartitionCursor, PartitionedBatchReader,
            PartitionedIndexedZSet, RelOffset,
        },
        trace::{
            DelayedTraceId, IntegrateTraceId, TraceBound, TraceBounds, UntimedTraceAppend, Z1Trace,
        },
        Aggregator, FilterMap,
    },
    trace::{Builder, Cursor, Spine},
    Circuit, DBData, DBWeight, RootCircuit, Stream,
};
use num::{Bounded, PrimInt};
use std::{borrow::Cow, marker::PhantomData, ops::Neg};

// TODO: `Default` trait bounds in this module are due to an implementation
// detail and can in principle be avoided.

pub type OrdPartitionedOverStream<PK, TS, A, R> =
    Stream<RootCircuit, OrdPartitionedIndexedZSet<PK, TS, Option<A>, R>>;

/// `Aggregator` object that computes a linear aggregation function.
// TODO: we need this because we currently compute linear aggregates
// using the same algorithm as general aggregates.  Additional performance
// gains can be obtained with an optimized implementation of radix trees
// for linear aggregates (specifically, updating a node when only
// some of its children have changed can be done without computing
// the sum of all children from scratch).
struct LinearAggregator<V, R, A, O, F, OF> {
    f: F,
    output_func: OF,
    phantom: PhantomData<(V, R, A, O)>,
}

impl<V, R, A, O, F, OF> Clone for LinearAggregator<V, R, A, O, F, OF>
where
    F: Clone,
    OF: Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            output_func: self.output_func.clone(),
            phantom: PhantomData,
        }
    }
}

impl<V, R, A, O, F, OF> LinearAggregator<V, R, A, O, F, OF> {
    fn new(f: F, output_func: OF) -> Self {
        Self {
            f,
            output_func,
            phantom: PhantomData,
        }
    }
}

impl<V, R, A, O, F, OF> Aggregator<V, (), R> for LinearAggregator<V, R, A, O, F, OF>
where
    V: DBData,
    R: DBWeight + ZRingValue,
    A: DBData + MulByRef<R, Output = A> + GroupValue,
    O: DBData,
    F: Fn(&V) -> A + Clone + 'static,
    OF: Fn(A) -> O + Clone + 'static,
{
    type Accumulator = A;
    type Output = O;

    type Semigroup = DefaultSemigroup<A>;

    fn aggregate<'s, C>(&self, cursor: &mut C) -> Option<A>
    where
        C: Cursor<'s, V, (), (), R>,
    {
        let mut res: Option<A> = None;

        while cursor.key_valid() {
            let w = cursor.weight();
            let new = (self.f)(cursor.key()).mul_by_ref(&w);
            res = match res {
                None => Some(new),
                Some(old) => Some(old + new),
            };
            cursor.step_key();
        }
        res
    }

    fn finalize(&self, accumulator: Self::Accumulator) -> Self::Output {
        (self.output_func)(accumulator)
    }
}

impl<B> Stream<RootCircuit, B>
where
    B: IndexedZSet,
{
    /// Similar to
    /// [`partitioned_rolling_aggregate`](`Stream::partitioned_rolling_aggregate`),
    /// but uses `watermark` to bound its memory footprint.
    ///
    /// Splits the input stream into non-overlapping
    /// partitions using `partition_func` and for each input record
    /// computes an aggregate over a relative time range (e.g., the
    /// last three months) within its partition.  Outputs the contents
    /// of the input stream extended with the value of the aggregate.
    ///
    /// This operator is incremental and will update previously
    /// computed outputs affected by new data.  For example,
    /// a data point arriving out-of-order may affect previously
    /// computed rolling aggregate values at future times.
    ///
    /// The `watermark` stream bounds the out-of-ordedness of the input
    /// data by providing a monotonically growing lower bound on
    /// timestamps that can appear in the input stream.  The operator
    /// does not expect inputs with timestamps smaller than the current
    /// watermark.  The `watermark` value is used to bound the amount of
    /// state maintained by the operator.
    ///
    /// # Background
    ///
    /// The rolling aggregate operator is typically applied to time series data
    /// with bounded out-of-ordedness, i.e, having seen a timestamp `ts` in the
    /// input stream, the operator will never observe a timestamp smaller than
    /// `ts - b` for some bound `b`.  This in turn means that the value of the
    /// aggregate will remain constant for timestamps that only depend on times
    /// `< ts - b`.  Hence, we do not need to maintain the state needed to
    /// recompute these aggregates, which allows us to bound the amount of state
    /// maintained by this operator.
    ///
    /// The bound `ts - b` is known as "watermark" and can be computed, e.g., by
    /// the [`watermark_monotonic`](`Stream::watermark_monotonic`) operator.
    ///
    /// # Arguments
    ///
    /// * `self` - time series data indexed by time.
    /// * `watermark` - monotonically growing lower bound on timestamps in the
    ///   input stream.
    /// * `partition_func` - function used to split inputs into non-overlapping
    ///   partitions indexed by partition key of type `PK`.
    /// * `aggregator` - aggregator used to summarize values within the relative
    ///   time range `range` of each input timestamp.
    /// * `range` - relative time range to aggregate over.
    pub fn partitioned_rolling_aggregate_with_watermark<PK, TS, V, Agg, PF>(
        &self,
        watermark: &Stream<RootCircuit, TS>,
        partition_func: PF,
        aggregator: Agg,
        range: RelRange<TS>,
    ) -> OrdPartitionedOverStream<PK, TS, Agg::Output, B::R>
    where
        B: IndexedZSet<Key = TS>,
        Self: for<'a> FilterMap<RootCircuit, ItemRef<'a> = (&'a B::Key, &'a B::Val), R = B::R>,
        B::R: ZRingValue,
        PK: DBData,
        PF: Fn(&B::Val) -> (PK, V) + Clone + 'static,
        Agg: Aggregator<V, (), B::R>,
        Agg::Accumulator: Default,
        TS: DBData + PrimInt,
        V: DBData,
    {
        self.circuit()
            .region("partitioned_rolling_aggregate_with_watermark", || {
                // Shift the aggregation window so that its right end is at 0.
                let shifted_range =
                    RelRange::new(range.from - range.to, RelOffset::Before(TS::zero()));

                // Trace bound used inside `partitioned_rolling_aggregate_inner` to
                // bound its output trace.  This is the same bound we use to construct
                // the input window here.
                let bound: TraceBound<(TS, Option<Agg::Output>)> = TraceBound::new();
                let bound_clone = bound.clone();

                // Restrict the input stream to the `[lb -> ∞)` time window,
                // where `lb = watermark - (range.to - range.from)` is the lower
                // bound on input timestamps that may be used to compute
                // changes to the rolling aggregate operator.
                let bounds = watermark.apply(move |wm| {
                    let lower = shifted_range
                        .range_of(wm)
                        .map(|range| range.from)
                        .unwrap_or_else(|| Bounded::min_value());
                    bound_clone.set((lower, None));
                    (lower, Bounded::max_value())
                });
                let window = self.window(&bounds);

                // Now that we've truncated old inputs, which required the
                // input stream to be indexed by time, we can re-index it
                // by partition id.
                let partition_func_clone = partition_func.clone();

                let partitioned_window = window.map_index(move |(ts, v)| {
                    let (partition_key, val) = partition_func_clone(v);
                    (partition_key, (*ts, val))
                });
                let partitioned_self = self.map_index(move |(ts, v)| {
                    let (partition_key, val) = partition_func(v);
                    (partition_key, (*ts, val))
                });

                partitioned_self.partitioned_rolling_aggregate_inner(
                    &partitioned_window,
                    aggregator,
                    range,
                    bound,
                )
            })
    }
}

impl<B> Stream<RootCircuit, B> {
    /// Rolling aggregate of a partitioned stream over time range.
    ///
    /// For each record in the input stream, computes an aggregate
    /// over a relative time range (e.g., the last three months).
    /// Outputs the contents of the input stream extended with the
    /// value of the aggregate.
    ///
    /// This operator is incremental and will update previously
    /// computed outputs affected by new data.  For example,
    /// a data point arriving out-of-order may affect previously
    /// computed rolling aggregate value at future times.
    pub fn partitioned_rolling_aggregate<TS, V, Agg>(
        &self,
        aggregator: Agg,
        range: RelRange<TS>,
    ) -> OrdPartitionedOverStream<B::Key, TS, Agg::Output, B::R>
    where
        B: PartitionedIndexedZSet<TS, V>,
        B::R: ZRingValue,
        Agg: Aggregator<V, (), B::R>,
        Agg::Accumulator: Default,
        TS: DBData + PrimInt,
        V: DBData,
    {
        self.partitioned_rolling_aggregate_generic::<TS, V, Agg, _>(aggregator, range)
    }

    /// Like [`Self::partitioned_rolling_aggregate`], but can return any
    /// batch type.
    pub fn partitioned_rolling_aggregate_generic<TS, V, Agg, O>(
        &self,
        aggregator: Agg,
        range: RelRange<TS>,
    ) -> Stream<RootCircuit, O>
    where
        B: PartitionedIndexedZSet<TS, V>,
        B::R: ZRingValue,
        Agg: Aggregator<V, (), B::R>,
        Agg::Accumulator: Default,
        O: PartitionedIndexedZSet<TS, Option<Agg::Output>, Key = B::Key, R = B::R>,
        TS: DBData + PrimInt,
        V: DBData,
    {
        // ```
        //                  ┌───────────────┐   input_trace
        //      ┌──────────►│integrate_trace├──────────────┐                              output
        //      │           └───────────────┘              │                           ┌────────────────────────────────────►
        //      │                                          ▼                           │
        // self │    ┌──────────────────────────┐  tree  ┌───────────────────────────┐ │  ┌──────────────────┐ output_trace
        // ─────┼───►│partitioned_tree_aggregate├───────►│PartitionedRollingAggregate├─┴──┤UntimedTraceAppend├────────┐
        //      │    └──────────────────────────┘        └───────────────────────────┘    └──────────────────┘        │
        //      │                                          ▲               ▲                 ▲                        │
        //      └──────────────────────────────────────────┘               │                 │                        │
        //                                                                 │               ┌─┴──┐                     │
        //                                                                 └───────────────┤Z^-1│◄────────────────────┘
        //                                                            output_trace_delayed └────┘
        // ```
        self.circuit().region("partitioned_rolling_aggregate", || {
            self.partitioned_rolling_aggregate_inner(self, aggregator, range, TraceBound::new())
        })
    }

    pub fn partitioned_rolling_aggregate_inner<TS, V, Agg, O>(
        &self,
        self_window: &Self,
        aggregator: Agg,
        range: RelRange<TS>,
        bound: TraceBound<(TS, Option<Agg::Output>)>,
    ) -> Stream<RootCircuit, O>
    where
        B: PartitionedIndexedZSet<TS, V>,
        B::R: ZRingValue,
        Agg: Aggregator<V, (), B::R>,
        Agg::Accumulator: Default,
        O: PartitionedIndexedZSet<TS, Option<Agg::Output>, Key = B::Key, R = B::R>,
        TS: DBData + PrimInt,
        V: DBData,
    {
        let circuit = self.circuit();
        let stream = self.shard();
        let stream_window = self_window.shard();

        // Build the radix tree over the bounded window.
        let tree = stream_window
            .partitioned_tree_aggregate::<TS, V, Agg>(aggregator.clone())
            .integrate_trace();
        let input_trace = stream_window.integrate_trace();

        // Truncate timestamps `< bound` in the output trace.
        let bounds = TraceBounds::new();
        bounds.add_key_bound(TraceBound::new());
        bounds.add_val_bound(bound);

        let (output_trace_delayed, z1feedback) = circuit.add_feedback(<Z1Trace<Spine<O>>>::new(
            false,
            circuit.root_scope(),
            bounds,
        ));
        output_trace_delayed.mark_sharded();

        let output = circuit
            .add_quaternary_operator(
                <PartitionedRollingAggregate<TS, V, Agg>>::new(range, aggregator),
                &stream,
                &input_trace,
                &tree,
                &output_trace_delayed,
            )
            .mark_sharded();

        let output_trace = circuit
            .add_binary_operator_with_preference(
                <UntimedTraceAppend<Spine<O>>>::new(),
                (
                    &output_trace_delayed,
                    OwnershipPreference::STRONGLY_PREFER_OWNED,
                ),
                (&output, OwnershipPreference::PREFER_OWNED),
            )
            .mark_sharded();

        z1feedback
            .connect_with_preference(&output_trace, OwnershipPreference::STRONGLY_PREFER_OWNED);

        circuit.cache_insert(
            DelayedTraceId::new(output_trace.origin_node_id().clone()),
            output_trace_delayed,
        );
        let bounds = <TraceBounds<O::Key, O::Val>>::unbounded();
        circuit.cache_insert(
            IntegrateTraceId::new(output.origin_node_id().clone()),
            (output_trace, bounds),
        );

        output
    }

    /// A version of [`Self::partitioned_rolling_aggregate`] optimized for
    /// linear aggregation functions.
    ///
    /// This method only works for linear aggregation functions `f`, i.e.,
    /// functions that satisfy `f(a+b) = f(a) + f(b)`.  It will produce
    /// incorrect results if `f` is not linear.
    pub fn partitioned_rolling_aggregate_linear<TS, V, A, O, F, OF>(
        &self,
        f: F,
        output_func: OF,
        range: RelRange<TS>,
    ) -> OrdPartitionedOverStream<B::Key, TS, O, B::R>
    where
        B: PartitionedIndexedZSet<TS, V>,
        B::R: ZRingValue,
        A: DBData + MulByRef<B::R, Output = A> + GroupValue + Default,
        F: Fn(&V) -> A + Clone + 'static,
        OF: Fn(A) -> O + Clone + 'static,
        TS: DBData + PrimInt,
        V: DBData,
        O: DBData,
    {
        let aggregator = LinearAggregator::new(f, output_func);
        self.partitioned_rolling_aggregate_generic::<TS, V, _, _>(aggregator, range)
    }

    /// Like [`Self::partitioned_rolling_aggregate_linear`], but can return any
    /// batch type.
    pub fn partitioned_rolling_aggregate_linear_generic<TS, V, A, O, F, OF, Out>(
        &self,
        f: F,
        output_func: OF,
        range: RelRange<TS>,
    ) -> Stream<RootCircuit, Out>
    where
        B: PartitionedIndexedZSet<TS, V>,
        B::R: ZRingValue,
        A: DBData + MulByRef<B::R, Output = A> + GroupValue + Default,
        F: Fn(&V) -> A + Clone + 'static,
        OF: Fn(A) -> O + Clone + 'static,
        TS: DBData + PrimInt,
        V: DBData,
        O: DBData,
        Out: PartitionedIndexedZSet<TS, Option<O>, Key = B::Key, R = B::R>,
    {
        let aggregator = LinearAggregator::new(f, output_func);
        self.partitioned_rolling_aggregate_generic::<TS, V, _, _>(aggregator, range)
    }
}

/// Quaternary operator that implements the internals of
/// `partitioned_rolling_aggregate`.
///
/// * Input stream 1: updates to the time series.  Used to identify affected
///   partitions and times.
/// * Input stream 2: trace containing the accumulated time series data.
/// * Input stream 3: trace containing the partitioned radix tree over the input
///   time series.
/// * Input stream 4: trace of previously produced outputs.  Used to compute
///   retractions.
struct PartitionedRollingAggregate<TS, V, Agg> {
    range: RelRange<TS>,
    aggregator: Agg,
    phantom: PhantomData<V>,
}

impl<TS, V, Agg> PartitionedRollingAggregate<TS, V, Agg> {
    fn new(range: RelRange<TS>, aggregator: Agg) -> Self {
        Self {
            range,
            aggregator,
            phantom: PhantomData,
        }
    }

    fn affected_ranges<'a, R, C>(&self, delta_cursor: &mut C) -> Ranges<TS>
    where
        C: Cursor<'a, TS, V, (), R>,
        TS: PrimInt,
    {
        let mut affected_ranges = Ranges::new();
        let mut delta_ranges = Ranges::new();

        while delta_cursor.key_valid() {
            if let Some(range) = self.range.affected_range_of(delta_cursor.key()) {
                affected_ranges.push_monotonic(range);
            }
            // If `delta_cursor.key()` is a new key that doesn't yet occur in the input
            // z-set, we need to compute its aggregate even if it is outside
            // affected range.
            delta_ranges.push_monotonic(Range::new(*delta_cursor.key(), *delta_cursor.key()));
            delta_cursor.step_key();
        }

        affected_ranges.merge(&delta_ranges)
    }
}

impl<TS, V, Agg> Operator for PartitionedRollingAggregate<TS, V, Agg>
where
    TS: 'static,
    V: 'static,
    Agg: 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::from("PartitionedRollingAggregate")
    }

    fn fixedpoint(&self, _scope: Scope) -> bool {
        true
    }
}

impl<TS, V, Agg, B, T, RT, OT, O> QuaternaryOperator<B, T, RT, OT, O>
    for PartitionedRollingAggregate<TS, V, Agg>
where
    TS: DBData + PrimInt,
    V: DBData,
    Agg: Aggregator<V, (), B::R>,
    B: PartitionedBatchReader<TS, V> + Clone,
    B::R: ZRingValue,
    T: PartitionedBatchReader<TS, V, Key = B::Key, R = B::R> + Clone,
    RT: PartitionedRadixTreeReader<TS, Agg::Accumulator, Key = B::Key> + Clone,
    OT: PartitionedBatchReader<TS, Option<Agg::Output>, Key = B::Key, R = B::R> + Clone,
    O: IndexedZSet<Key = B::Key, Val = (TS, Option<Agg::Output>), R = B::R>,
{
    fn eval<'a>(
        &mut self,
        input_delta: Cow<'a, B>,
        input_trace: Cow<'a, T>,
        radix_tree: Cow<'a, RT>,
        output_trace: Cow<'a, OT>,
    ) -> O {
        let mut delta_cursor = input_delta.cursor();
        let mut output_trace_cursor = output_trace.cursor();
        let mut input_trace_cursor = input_trace.cursor();
        let mut tree_cursor = radix_tree.cursor();

        let mut retraction_builder = O::Builder::new_builder(());
        let mut insertion_builder = O::Builder::with_capacity((), input_delta.len());

        // println!("delta: {input_delta:#x?}");
        // println!("radix tree: {radix_tree:#x?}");
        // println!("aggregate_range({range:x?})");
        // let mut treestr = String::new();
        // radix_tree.cursor().format_tree(&mut treestr).unwrap();
        // println!("tree: {treestr}");
        // tree_partition_cursor.rewind_keys();

        // Iterate over affected partitions.
        while delta_cursor.key_valid() {
            // Compute affected intervals using `input_delta`.
            let ranges = self.affected_ranges(&mut PartitionCursor::new(&mut delta_cursor));
            // println!("affected_ranges: {ranges:?}");

            // Clear old outputs.
            output_trace_cursor.seek_key(delta_cursor.key());
            if output_trace_cursor.key_valid() && output_trace_cursor.key() == delta_cursor.key() {
                let mut range_cursor = RangeCursor::new(
                    PartitionCursor::new(&mut output_trace_cursor),
                    ranges.clone(),
                );
                while range_cursor.key_valid() {
                    while range_cursor.val_valid() {
                        let weight = range_cursor.weight();
                        if !weight.is_zero() {
                            // println!("retract: ({:?}, ({:?}, {:?})) ", delta_cursor.key(),
                            // range_cursor.key(), range_cursor.val());
                            retraction_builder.push((
                                O::item_from(
                                    delta_cursor.key().clone(),
                                    (*range_cursor.key(), range_cursor.val().clone()),
                                ),
                                weight.neg(),
                            ));
                        }
                        range_cursor.step_val();
                    }
                    range_cursor.step_key();
                }
            };

            // Compute new outputs.
            input_trace_cursor.seek_key(delta_cursor.key());
            tree_cursor.seek_key(delta_cursor.key());

            if input_trace_cursor.key_valid() && input_trace_cursor.key() == delta_cursor.key() {
                debug_assert!(tree_cursor.key_valid());
                debug_assert_eq!(tree_cursor.key(), delta_cursor.key());

                let mut tree_partition_cursor = PartitionCursor::new(&mut tree_cursor);
                let mut input_range_cursor =
                    RangeCursor::new(PartitionCursor::new(&mut input_trace_cursor), ranges);

                // For all affected times, seek them in `input_trace`, compute aggregates using
                // using radix_tree.
                while input_range_cursor.key_valid() {
                    let range = if let Some(range) = self.range.range_of(input_range_cursor.key()) {
                        range
                    } else {
                        input_range_cursor.step_key();
                        continue;
                    };
                    tree_partition_cursor.rewind_keys();

                    // println!("aggregate_range({range:x?})");
                    // let mut treestr = String::new();
                    // tree_partition_cursor.format_tree(&mut treestr).unwrap();
                    // println!("tree: {treestr}");
                    // tree_partition_cursor.rewind_keys();

                    while input_range_cursor.val_valid() {
                        // Generate output update.
                        if !input_range_cursor.weight().le0() {
                            let agg = tree_partition_cursor
                                .aggregate_range::<Agg::Semigroup>(&range)
                                .map(|acc| self.aggregator.finalize(acc));
                            // println!("key: {:?}, range: {:?}, agg: {:?}",
                            // input_range_cursor.key(), range, agg);

                            insertion_builder.push((
                                O::item_from(
                                    delta_cursor.key().clone(),
                                    (*input_range_cursor.key(), agg),
                                ),
                                HasOne::one(),
                            ));
                            break;
                        }

                        input_range_cursor.step_val();
                    }

                    input_range_cursor.step_key();
                }
            }

            delta_cursor.step_key();
        }

        let retractions = retraction_builder.done();
        let insertions = insertion_builder.done();
        retractions.add(insertions)
    }
}

#[cfg(test)]
mod test {
    use crate::{
        algebra::DefaultSemigroup,
        operator::{
            time_series::{
                range::{Range, RelOffset, RelRange},
                PartitionCursor,
            },
            trace::TraceBound,
            FilterMap, Fold,
        },
        trace::{Batch, BatchReader, Cursor},
        CollectionHandle, DBSPHandle, OrdIndexedZSet, RootCircuit, Runtime, Stream,
    };
    use size_of::SizeOf;

    type DataBatch = OrdIndexedZSet<u64, (u64, i64), isize>;
    type DataStream = Stream<RootCircuit, DataBatch>;
    type OutputBatch = OrdIndexedZSet<u64, (u64, Option<i64>), isize>;
    type OutputStream = Stream<RootCircuit, OutputBatch>;

    // Reference implementation of `aggregate_range` for testing.
    fn aggregate_range_slow(batch: &DataBatch, partition: u64, range: Range<u64>) -> Option<i64> {
        let mut cursor = batch.cursor();

        cursor.seek_key(&partition);
        assert!(cursor.key_valid());
        assert!(*cursor.key() == partition);
        let mut partition_cursor = PartitionCursor::new(&mut cursor);

        let mut agg = None;
        partition_cursor.seek_key(&range.from);
        while partition_cursor.key_valid() && *partition_cursor.key() <= range.to {
            while partition_cursor.val_valid() {
                let w = partition_cursor.weight() as i64;
                agg = if let Some(a) = agg {
                    Some(a + *partition_cursor.val() * w)
                } else {
                    Some(*partition_cursor.val() * w)
                };
                partition_cursor.step_val();
            }
            partition_cursor.step_key();
        }

        agg
    }

    // Reference implementation of `partitioned_rolling_aggregate` for testing.
    fn partitioned_rolling_aggregate_slow(
        stream: &DataStream,
        range_spec: RelRange<u64>,
    ) -> OutputStream {
        stream
            .gather(0)
            .integrate()
            .apply(move |batch: &DataBatch| {
                let mut tuples = Vec::with_capacity(batch.len());

                let mut cursor = batch.cursor();

                while cursor.key_valid() {
                    while cursor.val_valid() {
                        let partition = *cursor.key();
                        let (ts, _val) = *cursor.val();
                        let range = if let Some(range) = range_spec.range_of(&ts) {
                            range
                        } else {
                            cursor.step_val();
                            continue;
                        };
                        let agg = aggregate_range_slow(batch, partition, range);
                        tuples.push(((partition, (ts, agg)), 1));
                        cursor.step_val();
                    }
                    cursor.step_key();
                }

                OutputBatch::from_tuples((), tuples)
            })
            .stream_distinct()
            .gather(0)
    }

    type RangeHandle = CollectionHandle<u64, ((u64, i64), isize)>;

    fn partition_rolling_aggregate_circuit(
        lateness: u64,
        size_bound: Option<usize>,
    ) -> (DBSPHandle, RangeHandle) {
        Runtime::init_circuit(4, move |circuit| {
            let (input_stream, input_handle) =
                circuit.add_input_indexed_zset::<u64, (u64, i64), isize>();

            let input_by_time =
                input_stream.map_index(|(partition, (ts, val))| (*ts, (*partition, *val)));

            let watermark =
                input_by_time.watermark_monotonic(move |ts| ts.saturating_sub(lateness));

            let aggregator = <Fold<_, DefaultSemigroup<_>, _, _>>::new(
                0i64,
                |agg: &mut i64, val: &i64, w: isize| *agg += val * (w as i64),
            );

            let range_spec = RelRange::new(RelOffset::Before(1000), RelOffset::Before(0));
            let expected_1000_0 = partitioned_rolling_aggregate_slow(&input_stream, range_spec);
            let output_1000_0 = input_stream
                .partitioned_rolling_aggregate::<u64, i64, _>(aggregator.clone(), range_spec)
                .gather(0)
                .integrate();
            expected_1000_0.apply2(&output_1000_0, |expected, actual| {
                assert_eq!(expected, actual)
            });

            let output_1000_0_watermark = input_by_time
                .partitioned_rolling_aggregate_with_watermark(
                    &watermark,
                    |(partition, val)| (*partition, *val),
                    aggregator.clone(),
                    range_spec.clone(),
                )
                .gather(0)
                .integrate();

            expected_1000_0.apply2(&output_1000_0_watermark, |expected, actual| {
                assert_eq!(expected, actual)
            });

            let output_1000_0_linear = input_stream
                .partitioned_rolling_aggregate_linear::<u64, i64, _, _, _, _>(
                    |v| *v,
                    |v| v,
                    range_spec,
                )
                .gather(0)
                .integrate();
            expected_1000_0.apply2(&output_1000_0_linear, |expected, actual| {
                assert_eq!(expected, actual)
            });

            let range_spec = RelRange::new(RelOffset::Before(500), RelOffset::After(500));
            let expected_500_500 = partitioned_rolling_aggregate_slow(&input_stream, range_spec);
            let aggregate_500_500 = input_stream
                .partitioned_rolling_aggregate::<u64, i64, _>(aggregator.clone(), range_spec);
            let output_500_500 = aggregate_500_500.gather(0).integrate();
            expected_500_500.apply2(&output_500_500, |expected, actual| {
                assert_eq!(expected, actual)
            });

            let aggregate_500_500_watermark = input_by_time
                .partitioned_rolling_aggregate_with_watermark(
                    &watermark,
                    |(partition, val)| (*partition, *val),
                    aggregator.clone(),
                    range_spec.clone(),
                );
            let output_500_500_watermark = aggregate_500_500_watermark.gather(0).integrate();

            let bound = TraceBound::new();
            bound.set((u64::max_value(), None));

            aggregate_500_500_watermark
                .integrate_trace_with_bound(TraceBound::new(), bound.clone())
                .apply(move |trace| {
                    if let Some(bound) = size_bound {
                        assert!(trace.size_of().total_bytes() <= bound);
                    }
                    ()
                });

            expected_500_500.apply2(&output_500_500_watermark, |expected, actual| {
                assert_eq!(expected, actual)
            });

            let output_500_500_linear = input_stream
                .partitioned_rolling_aggregate_linear::<u64, i64, _, _, _, _>(
                    |v| *v,
                    |v| v,
                    range_spec,
                )
                .gather(0)
                .integrate();
            expected_500_500.apply2(&output_500_500_linear, |expected, actual| {
                assert_eq!(expected, actual)
            });

            let range_spec = RelRange::new(RelOffset::Before(500), RelOffset::Before(100));
            let expected_500_100 = partitioned_rolling_aggregate_slow(&input_stream, range_spec);
            let output_500_100 = input_stream
                .partitioned_rolling_aggregate::<u64, i64, _>(aggregator, range_spec)
                .gather(0)
                .integrate();
            expected_500_100.apply2(&output_500_100, |expected, actual| {
                assert_eq!(expected, actual)
            });

            input_handle
        })
        .unwrap()
    }

    #[test]
    fn test_partitioned_over_range_2() {
        let (mut circuit, mut input) = partition_rolling_aggregate_circuit(u64::max_value(), None);

        circuit.step().unwrap();

        input.append(&mut vec![(2, ((110271, 100), 1))]);
        circuit.step().unwrap();

        input.append(&mut vec![(2, ((0, 100), 1))]);
        circuit.step().unwrap();

        circuit.kill().unwrap();
    }

    #[test]
    fn test_partitioned_over_range() {
        let (mut circuit, mut input) = partition_rolling_aggregate_circuit(u64::max_value(), None);

        circuit.step().unwrap();

        input.append(&mut vec![
            (0, ((1, 100), 1)),
            (0, ((10, 100), 1)),
            (0, ((20, 100), 1)),
            (0, ((30, 100), 1)),
        ]);
        circuit.step().unwrap();

        input.append(&mut vec![
            (0, ((5, 100), 1)),
            (0, ((15, 100), 1)),
            (0, ((25, 100), 1)),
            (0, ((35, 100), 1)),
        ]);
        circuit.step().unwrap();

        input.append(&mut vec![
            (0, ((1, 100), -1)),
            (0, ((10, 100), -1)),
            (0, ((20, 100), -1)),
            (0, ((30, 100), -1)),
        ]);
        input.append(&mut vec![
            (1, ((1, 100), 1)),
            (1, ((1000, 100), 1)),
            (1, ((2000, 100), 1)),
            (1, ((3000, 100), 1)),
        ]);
        circuit.step().unwrap();

        circuit.kill().unwrap();
    }

    use proptest::{collection, prelude::*};

    type InputTuple = (u64, ((u64, i64), isize));
    type InputBatch = Vec<InputTuple>;

    fn input_tuple(partitions: u64, window: (u64, u64)) -> impl Strategy<Value = InputTuple> {
        (
            (0..partitions),
            ((window.0..window.1, 100..101i64), 1..2isize),
        )
    }

    fn input_batch(
        partitions: u64,
        window: (u64, u64),
        max_batch_size: usize,
    ) -> impl Strategy<Value = InputBatch> {
        collection::vec(input_tuple(partitions, window), 0..max_batch_size)
    }

    fn input_trace(
        partitions: u64,
        epoch: u64,
        max_batch_size: usize,
        max_batches: usize,
    ) -> impl Strategy<Value = Vec<InputBatch>> {
        collection::vec(
            input_batch(partitions, (0, epoch), max_batch_size),
            0..max_batches,
        )
    }

    fn input_trace_quasi_monotone(
        partitions: u64,
        window_size: u64,
        window_step: u64,
        max_batch_size: usize,
        batches: usize,
    ) -> impl Strategy<Value = Vec<InputBatch>> {
        (0..batches)
            .map(|i| {
                input_batch(
                    partitions,
                    (i as u64 * window_step, i as u64 * window_step + window_size),
                    max_batch_size,
                )
                .boxed()
            })
            .collect::<Vec<_>>()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5))]

        #[test]
        #[cfg_attr(feature = "persistence", ignore = "takes a long time?")]
        fn proptest_partitioned_rolling_aggregate_quasi_monotone(trace in input_trace_quasi_monotone(5, 10_000, 2_000, 20, 200)) {
            // 10_000 is an empirically established bound: without GC this test needs >10KB.
            let (mut circuit, mut input) = partition_rolling_aggregate_circuit(10000, Some(10_000));

            for mut batch in trace {
                input.append(&mut batch);
                circuit.step().unwrap();
            }

            circuit.kill().unwrap();
        }
    }

    proptest! {
        #[test]
        #[cfg_attr(feature = "persistence", ignore = "takes a long time?")]
        fn proptest_partitioned_over_range_sparse(trace in input_trace(5, 1_000_000, 20, 20)) {
            let (mut circuit, mut input) = partition_rolling_aggregate_circuit(u64::max_value(), None);

            for mut batch in trace {
                input.append(&mut batch);
                circuit.step().unwrap();
            }

            circuit.kill().unwrap();
        }

        #[test]
        #[cfg_attr(feature = "persistence", ignore = "takes a long time?")]
        fn proptest_partitioned_over_range_dense(trace in input_trace(5, 1_000, 50, 20)) {
            let (mut circuit, mut input) = partition_rolling_aggregate_circuit(u64::max_value(), None);

            for mut batch in trace {
                input.append(&mut batch);
                circuit.step().unwrap();
            }

            circuit.kill().unwrap();
        }
    }
}
