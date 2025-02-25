mod nodes;
mod operators;
mod tests;

use crate::{
    codegen::{Codegen, CodegenConfig, LayoutVTable, NativeLayout, NativeLayoutCache, VTable},
    dataflow::nodes::{
        Antijoin, DataflowSubgraph, DelayedFeedback, Delta0, Differentiate, Distinct, Export,
        FilterFn, FilterMap, FilterMapIndex, FlatMap, FlatMapFn, Fold, Integrate, JoinCore, MapFn,
        Max, Min, Minus, Noop, PartitionedRollingFold,
    },
    ir::{
        graph,
        literal::{NullableConstant, RowLiteral, StreamCollection},
        nodes::{DataflowNode as _, Node, StreamKind, StreamLayout, Subgraph as SubgraphNode},
        Constant, Graph, GraphExt, LayoutId, NodeId,
    },
    row::{Row, UninitRow},
    ThinStr,
};
use cranelift_jit::JITModule;
use cranelift_module::FuncId;
use dbsp::{
    algebra::UnimplementedSemigroup,
    operator::{FilterMap as _, Generator},
    trace::{Batch, BatchReader, Batcher, Cursor, Spine},
    Circuit, CollectionHandle, DBTimestamp, OrdIndexedZSet, OrdZSet, OutputHandle, RootCircuit,
    Stream,
};
use derive_more::{IsVariant, Unwrap};
use nodes::{
    DataflowNode, Filter, IndexWith, Map, MonotonicJoin, Neg, Sink, Source, SourceMap, Sum,
};
use petgraph::{algo, prelude::DiGraphMap};
use std::{collections::BTreeMap, iter, mem::transmute, ptr::NonNull};

// TODO: Keep layout ids in dataflow nodes so we can do assertions that types
// are correct

type RowSet = OrdZSet<Row, i32>;
type RowMap = OrdIndexedZSet<Row, Row, i32>;

type Inputs = BTreeMap<NodeId, RowInput>;
type Outputs = BTreeMap<NodeId, RowOutput>;

#[derive(Clone, IsVariant, Unwrap)]
pub enum RowInput {
    Set(CollectionHandle<Row, i32>),
    Map(CollectionHandle<Row, (Row, i32)>),
}

impl RowInput {
    pub fn as_set_mut(&mut self) -> Option<&mut CollectionHandle<Row, i32>> {
        if let Self::Set(handle) = self {
            Some(handle)
        } else {
            None
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut CollectionHandle<Row, (Row, i32)>> {
        if let Self::Map(handle) = self {
            Some(handle)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub enum RowOutput {
    Set(OutputHandle<RowSet>),
    Map(OutputHandle<RowMap>),
}

impl RowOutput {
    pub const fn as_set(&self) -> Option<&OutputHandle<RowSet>> {
        if let Self::Set(handle) = self {
            Some(handle)
        } else {
            None
        }
    }

    pub fn as_set_mut(&mut self) -> Option<&mut OutputHandle<RowSet>> {
        if let Self::Set(handle) = self {
            Some(handle)
        } else {
            None
        }
    }

    pub const fn as_map(&self) -> Option<&OutputHandle<RowMap>> {
        if let Self::Map(handle) = self {
            Some(handle)
        } else {
            None
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut OutputHandle<RowMap>> {
        if let Self::Map(handle) = self {
            Some(handle)
        } else {
            None
        }
    }
}

// TODO: Change the weight to a `Row`? Toggle between `i32` and `i64`?
#[derive(Clone, IsVariant, Unwrap)]
pub enum RowStream<C> {
    Set(Stream<C, RowSet>),
    Map(Stream<C, RowMap>),
}

impl<C> RowStream<C> {
    pub const fn as_set(&self) -> Option<&Stream<C, RowSet>> {
        if let Self::Set(set) = self {
            Some(set)
        } else {
            None
        }
    }

    pub const fn as_map(&self) -> Option<&Stream<C, RowMap>> {
        if let Self::Map(map) = self {
            Some(map)
        } else {
            None
        }
    }
}

#[derive(Clone, IsVariant, Unwrap)]
pub enum RowTrace<C> {
    Set(Stream<C, Spine<RowSet>>),
    Map(Stream<C, Spine<RowMap>>),
}

#[derive(Debug, Clone)]
pub enum RowZSet {
    Set(RowSet),
    Map(RowMap),
}

pub struct JitHandle {
    jit: JITModule,
    vtables: BTreeMap<LayoutId, *mut VTable>,
}

impl JitHandle {
    pub fn vtables(&self) -> &BTreeMap<LayoutId, *mut VTable> {
        &self.vtables
    }

    /// Free all memory associated with the JIT compiled code, including vtables
    /// and the functions themselves
    ///
    /// # Safety
    ///
    /// Cannot call this while any vtable or function pointers are still live or
    /// in use, any attempt to use them after calling this function is UB
    pub unsafe fn free_memory(mut self) {
        for &vtable in self.vtables.values() {
            drop(Box::from_raw(vtable));
        }

        self.vtables.clear();
        self.jit.free_memory();
    }
}

#[derive(Debug, Clone)]
pub struct CompiledDataflow {
    nodes: BTreeMap<NodeId, DataflowNode>,
    edges: DiGraphMap<NodeId, ()>,
}

impl CompiledDataflow {
    pub fn new(graph: &Graph, config: CodegenConfig) -> (Self, JitHandle, NativeLayoutCache) {
        let mut node_kinds = BTreeMap::new();
        let mut node_streams: BTreeMap<NodeId, Option<_>> = BTreeMap::new();

        let (mut inputs, mut input_nodes) = (Vec::with_capacity(16), Vec::with_capacity(16));
        let order = algo::toposort(graph.edges(), None).unwrap();
        for node_id in order {
            if !graph.nodes().contains_key(&node_id) {
                continue;
            }

            let node = &graph.nodes()[&node_id];

            node.inputs(&mut input_nodes);
            inputs.extend(input_nodes.iter().filter_map(|input| node_streams[input]));

            node_kinds.insert(node_id, node.output_kind(&inputs));
            node_streams.insert(node_id, node.output_stream(&inputs));

            inputs.clear();
            input_nodes.clear();

            if let Node::Subgraph(subgraph) = node {
                collect_subgraph_inputs(
                    subgraph,
                    &mut inputs,
                    &mut input_nodes,
                    &mut node_kinds,
                    &mut node_streams,
                );
            }
        }
        drop((inputs, input_nodes));

        fn collect_subgraph_inputs(
            graph: &SubgraphNode,
            inputs: &mut Vec<StreamLayout>,
            input_nodes: &mut Vec<NodeId>,
            node_kinds: &mut BTreeMap<NodeId, Option<StreamKind>>,
            node_streams: &mut BTreeMap<NodeId, Option<StreamLayout>>,
        ) {
            let order = algo::toposort(graph.edges(), None).unwrap();
            for node_id in order {
                if graph.input_nodes().contains_key(&node_id) {
                    continue;
                }

                let node = &graph.nodes()[&node_id];
                node.inputs(input_nodes);
                inputs.extend(input_nodes.iter().filter_map(|input| node_streams[input]));

                node_kinds.insert(node_id, node.output_kind(inputs));
                node_streams.insert(node_id, node.output_stream(inputs));

                inputs.clear();
                input_nodes.clear();

                if let Node::Subgraph(subgraph) = node {
                    collect_subgraph_inputs(
                        subgraph,
                        inputs,
                        input_nodes,
                        node_kinds,
                        node_streams,
                    );
                }
            }
        }

        fn collect_functions(
            codegen: &mut Codegen,
            functions: &mut BTreeMap<NodeId, Vec<FuncId>>,
            vtables: &mut BTreeMap<LayoutId, LayoutVTable>,
            graph: &graph::Subgraph,
        ) {
            for (&node_id, node) in graph.nodes() {
                match node {
                    Node::Map(map) => {
                        let map_fn =
                            codegen.codegen_func(&format!("map_fn_{node_id}"), map.map_fn());
                        functions.insert(node_id, vec![map_fn]);

                        for layout in [map.input_layout(), map.output_layout()] {
                            match layout {
                                StreamLayout::Set(key) => {
                                    vtables
                                        .entry(key)
                                        .or_insert_with(|| codegen.vtable_for(key));
                                }

                                StreamLayout::Map(key, value) => {
                                    vtables
                                        .entry(key)
                                        .or_insert_with(|| codegen.vtable_for(key));
                                    vtables
                                        .entry(value)
                                        .or_insert_with(|| codegen.vtable_for(value));
                                }
                            }
                        }
                    }

                    Node::Filter(filter) => {
                        let filter_fn = codegen
                            .codegen_func(&format!("filter_fn_{node_id}"), filter.filter_fn());
                        functions.insert(node_id, vec![filter_fn]);
                    }

                    Node::FilterMap(filter_map) => {
                        let fmap_fn = codegen.codegen_func(
                            &format!("filter_map_fn_{node_id}"),
                            filter_map.filter_map(),
                        );
                        functions.insert(node_id, vec![fmap_fn]);

                        vtables
                            .entry(filter_map.layout())
                            .or_insert_with(|| codegen.vtable_for(filter_map.layout()));
                    }

                    Node::Fold(fold) => {
                        let step_fn = codegen
                            .codegen_func(&format!("fold_step_fn_{node_id}"), fold.step_fn());
                        let finish_fn = codegen
                            .codegen_func(&format!("fold_finish_fn_{node_id}"), fold.finish_fn());
                        functions.insert(node_id, vec![step_fn, finish_fn]);
                    }

                    Node::PartitionedRollingFold(fold) => {
                        let step_fn = codegen.codegen_func(
                            &format!("partitioned_rolling_fold_step_fn_{node_id}"),
                            fold.step_fn(),
                        );
                        let finish_fn = codegen.codegen_func(
                            &format!("partitioned_rolling_fold_finish_fn_{node_id}"),
                            fold.finish_fn(),
                        );
                        functions.insert(node_id, vec![step_fn, finish_fn]);
                    }

                    Node::FlatMap(flat_map) => {
                        let flat_map_fn = codegen
                            .codegen_func(&format!("flat_map_fn_{node_id}"), flat_map.flat_map());
                        functions.insert(node_id, vec![flat_map_fn]);

                        match flat_map.output_layout() {
                            StreamLayout::Set(key) => {
                                vtables
                                    .entry(key)
                                    .or_insert_with(|| codegen.vtable_for(key));
                            }

                            StreamLayout::Map(key, value) => {
                                vtables
                                    .entry(key)
                                    .or_insert_with(|| codegen.vtable_for(key));
                                vtables
                                    .entry(value)
                                    .or_insert_with(|| codegen.vtable_for(value));
                            }
                        }
                    }

                    Node::IndexWith(index_with) => {
                        let index_fn = codegen
                            .codegen_func(&format!("index_fn_{node_id}"), index_with.index_fn());
                        functions.insert(node_id, vec![index_fn]);

                        vtables
                            .entry(index_with.key_layout())
                            .or_insert_with(|| codegen.vtable_for(index_with.key_layout()));
                        vtables
                            .entry(index_with.value_layout())
                            .or_insert_with(|| codegen.vtable_for(index_with.value_layout()));
                    }

                    Node::Source(source) => {
                        vtables
                            .entry(source.layout())
                            .or_insert_with(|| codegen.vtable_for(source.layout()));
                    }

                    Node::SourceMap(source) => {
                        vtables
                            .entry(source.key())
                            .or_insert_with(|| codegen.vtable_for(source.key()));
                        vtables
                            .entry(source.value())
                            .or_insert_with(|| codegen.vtable_for(source.value()));
                    }

                    Node::JoinCore(join) => {
                        let join_fn =
                            codegen.codegen_func(&format!("join_fn_{node_id}"), join.join_fn());
                        functions.insert(node_id, vec![join_fn]);

                        vtables
                            .entry(join.key_layout())
                            .or_insert_with(|| codegen.vtable_for(join.key_layout()));
                        vtables
                            .entry(join.value_layout())
                            .or_insert_with(|| codegen.vtable_for(join.value_layout()));
                    }

                    Node::MonotonicJoin(join) => {
                        let join_fn = codegen
                            .codegen_func(&format!("monotonic_join_fn_{node_id}"), join.join_fn());
                        functions.insert(node_id, vec![join_fn]);

                        vtables
                            .entry(join.key_layout())
                            .or_insert_with(|| codegen.vtable_for(join.key_layout()));
                    }

                    Node::Subgraph(subgraph) => {
                        collect_functions(codegen, functions, vtables, subgraph.subgraph());
                    }

                    Node::Constant(constant) => match constant.layout() {
                        StreamLayout::Set(key) => {
                            vtables
                                .entry(key)
                                .or_insert_with(|| codegen.vtable_for(key));
                        }

                        StreamLayout::Map(key, value) => {
                            vtables
                                .entry(key)
                                .or_insert_with(|| codegen.vtable_for(key));
                            vtables
                                .entry(value)
                                .or_insert_with(|| codegen.vtable_for(value));
                        }
                    },

                    Node::Min(_)
                    | Node::Max(_)
                    | Node::Distinct(_)
                    | Node::Delta0(_)
                    | Node::DelayedFeedback(_)
                    | Node::Neg(_)
                    | Node::Sum(_)
                    | Node::Differentiate(_)
                    | Node::Integrate(_)
                    | Node::Sink(_)
                    | Node::Export(_)
                    | Node::ExportedNode(_)
                    | Node::Minus(_)
                    | Node::Antijoin(_) => {}
                }
            }
        }

        // Run codegen over all nodes
        let mut codegen = Codegen::new(graph.layout_cache().clone(), config);
        // TODO: SmallVec
        let mut node_functions = BTreeMap::new();
        let mut vtables = BTreeMap::new();
        collect_functions(
            &mut codegen,
            &mut node_functions,
            &mut vtables,
            graph.graph(),
        );

        fn compile_nodes(
            graph: &graph::Subgraph,
            vtables: &BTreeMap<LayoutId, *mut VTable>,
            jit: &JITModule,
            node_streams: &BTreeMap<NodeId, Option<StreamLayout>>,
            node_functions: &BTreeMap<NodeId, Vec<FuncId>>,
            layout_cache: &NativeLayoutCache,
        ) -> BTreeMap<NodeId, DataflowNode> {
            let mut nodes = BTreeMap::new();
            for (node_id, node) in graph.nodes() {
                let output = node_streams[node_id];

                match node {
                    Node::Map(map) => {
                        let input = map.input();
                        let map_fn = jit.get_finalized_function(node_functions[node_id][0]);

                        let map_fn = unsafe {
                            match (map.input_layout(), map.output_layout()) {
                                (StreamLayout::Set(_), StreamLayout::Set(key_layout)) => {
                                    MapFn::SetSet {
                                        map: transmute(map_fn),
                                        key_vtable: &*vtables[&key_layout],
                                    }
                                }

                                (
                                    StreamLayout::Set(_),
                                    StreamLayout::Map(key_layout, value_layout),
                                ) => MapFn::SetMap {
                                    map: transmute(map_fn),
                                    key_vtable: &*vtables[&key_layout],
                                    value_vtable: &*vtables[&value_layout],
                                },

                                (StreamLayout::Map(_, _), StreamLayout::Set(key_layout)) => {
                                    MapFn::MapSet {
                                        map: transmute(map_fn),
                                        key_vtable: &*vtables[&key_layout],
                                    }
                                }

                                (
                                    StreamLayout::Map(_, _),
                                    StreamLayout::Map(key_layout, value_layout),
                                ) => MapFn::MapMap {
                                    map: transmute(map_fn),
                                    key_vtable: &*vtables[&key_layout],
                                    value_vtable: &*vtables[&value_layout],
                                },
                            }
                        };
                        let map = DataflowNode::Map(Map { input, map_fn });

                        nodes.insert(*node_id, map);
                    }

                    Node::Filter(filter) => {
                        let filter_fn = jit.get_finalized_function(node_functions[node_id][0]);
                        let input = filter.input();

                        let (filter_fn, layout) = unsafe {
                            match output.unwrap() {
                                StreamLayout::Set(key) => {
                                    (FilterFn::Set(transmute(filter_fn)), StreamLayout::Set(key))
                                }
                                StreamLayout::Map(key, value) => (
                                    FilterFn::Map(transmute(filter_fn)),
                                    StreamLayout::Map(key, value),
                                ),
                            }
                        };
                        let filter = DataflowNode::Filter(Filter {
                            input,
                            filter_fn,
                            layout,
                        });

                        nodes.insert(*node_id, filter);
                    }

                    Node::FilterMap(filter_map) => {
                        let fmap_fn = jit.get_finalized_function(node_functions[node_id][0]);
                        let input = filter_map.input();
                        let output_vtable = unsafe { &*vtables[&filter_map.layout()] };

                        let node = match output.unwrap() {
                            StreamLayout::Set(_) => DataflowNode::FilterMap(FilterMap {
                                input,
                                filter_map: unsafe {
                                    transmute::<_, unsafe extern "C" fn(*const u8, *mut u8) -> bool>(
                                        fmap_fn,
                                    )
                                },
                                output_vtable,
                            }),

                            StreamLayout::Map(..) => DataflowNode::FilterMapIndex(FilterMapIndex {
                                input,
                                filter_map: unsafe {
                                    transmute::<
                                        _,
                                        unsafe extern "C" fn(*const u8, *const u8, *mut u8) -> bool,
                                    >(fmap_fn)
                                },
                                output_vtable,
                            }),
                        };

                        nodes.insert(*node_id, node);
                    }

                    Node::FlatMap(flat_map) => {
                        let flat_map_fn = jit.get_finalized_function(node_functions[node_id][0]);
                        let input_is_map = node_streams[&flat_map.input()].unwrap().is_map();
                        let flat_map = DataflowNode::FlatMap(FlatMap {
                            input: flat_map.input(),
                            flat_map: match flat_map.output_layout() {
                                StreamLayout::Set(key) => {
                                    let key_vtable = unsafe { &*vtables[&key] };

                                    if input_is_map {
                                        FlatMapFn::MapSet {
                                            flat_map: unsafe { transmute(flat_map_fn) },
                                            key_vtable,
                                        }
                                    } else {
                                        FlatMapFn::SetSet {
                                            flat_map: unsafe { transmute(flat_map_fn) },
                                            key_vtable,
                                        }
                                    }
                                }

                                StreamLayout::Map(key, value) => {
                                    let (key_vtable, value_vtable) =
                                        unsafe { (&*vtables[&key], &*vtables[&value]) };

                                    if input_is_map {
                                        FlatMapFn::MapMap {
                                            flat_map: unsafe { transmute(flat_map_fn) },
                                            key_vtable,
                                            value_vtable,
                                        }
                                    } else {
                                        FlatMapFn::SetMap {
                                            flat_map: unsafe { transmute(flat_map_fn) },
                                            key_vtable,
                                            value_vtable,
                                        }
                                    }
                                }
                            },
                        });
                        nodes.insert(*node_id, flat_map);
                    }

                    Node::Neg(neg) => {
                        nodes.insert(*node_id, DataflowNode::Neg(Neg { input: neg.input() }));
                    }

                    Node::Sum(sum) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Sum(Sum {
                                inputs: sum.inputs().to_vec(),
                            }),
                        );
                    }

                    Node::Minus(minus) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Minus(Minus {
                                lhs: minus.lhs(),
                                rhs: minus.rhs(),
                            }),
                        );
                    }

                    Node::Fold(fold) => {
                        let (acc_vtable, step_vtable, output_vtable) = unsafe {
                            (
                                &*vtables[&fold.acc_layout()],
                                &*vtables[&fold.step_layout()],
                                &*vtables[&fold.output_layout()],
                            )
                        };
                        let acc_layout = layout_cache.layout_of(fold.acc_layout());

                        let init =
                            unsafe { row_from_literal(fold.init(), acc_vtable, &acc_layout) };

                        let (step_fn, finish_fn) = (
                            jit.get_finalized_function(node_functions[node_id][0]),
                            jit.get_finalized_function(node_functions[node_id][1]),
                        );

                        let fold = DataflowNode::Fold(Fold {
                            input: fold.input(),
                            init,
                            acc_vtable,
                            step_vtable,
                            output_vtable,
                            step_fn: unsafe { transmute(step_fn) },
                            finish_fn: unsafe { transmute(finish_fn) },
                        });
                        nodes.insert(*node_id, fold);
                    }

                    Node::PartitionedRollingFold(fold) => {
                        let (acc_vtable, step_vtable, output_vtable) = unsafe {
                            (
                                &*vtables[&fold.acc_layout()],
                                &*vtables[&fold.step_layout()],
                                &*vtables[&fold.output_layout()],
                            )
                        };
                        let acc_layout = layout_cache.layout_of(fold.acc_layout());

                        let init =
                            unsafe { row_from_literal(fold.init(), acc_vtable, &acc_layout) };

                        let (step_fn, finish_fn) = (
                            jit.get_finalized_function(node_functions[node_id][0]),
                            jit.get_finalized_function(node_functions[node_id][1]),
                        );

                        let fold = DataflowNode::PartitionedRollingFold(PartitionedRollingFold {
                            input: fold.input(),
                            range: fold.range(),
                            init,
                            acc_vtable,
                            step_vtable,
                            output_vtable,
                            step_fn: unsafe { transmute(step_fn) },
                            finish_fn: unsafe { transmute(finish_fn) },
                        });
                        nodes.insert(*node_id, fold);
                    }

                    Node::Sink(sink) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Sink(Sink {
                                input: sink.input(),
                            }),
                        );
                    }

                    Node::Source(source) => {
                        let output_vtable = unsafe { &*vtables[&source.layout()] };
                        nodes.insert(*node_id, DataflowNode::Source(Source { output_vtable }));
                    }

                    Node::SourceMap(source) => {
                        let key_vtable = unsafe { &*vtables[&source.key()] };
                        let value_vtable = unsafe { &*vtables[&source.value()] };
                        nodes.insert(
                            *node_id,
                            DataflowNode::SourceMap(SourceMap {
                                key_vtable,
                                value_vtable,
                            }),
                        );
                    }

                    Node::IndexWith(index) => {
                        let input = index.input();
                        let index_fn = jit.get_finalized_function(node_functions[node_id][0]);
                        let key_vtable = unsafe { &*vtables[&index.key_layout()] };
                        let value_vtable = unsafe { &*vtables[&index.value_layout()] };

                        let node = match node_streams[&input].unwrap() {
                            StreamLayout::Set(_) => DataflowNode::IndexWith(IndexWith {
                                input,
                                index_fn: unsafe {
                                    transmute::<_, unsafe extern "C" fn(*const u8, *mut u8, *mut u8)>(
                                        index_fn,
                                    )
                                },
                                key_vtable,
                                value_vtable,
                            }),

                            StreamLayout::Map(..) => todo!(),
                        };

                        nodes.insert(*node_id, node);
                    }

                    Node::Differentiate(diff) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Differentiate(Differentiate {
                                input: diff.input(),
                            }),
                        );
                    }

                    Node::Integrate(integrate) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Integrate(Integrate {
                                input: integrate.input(),
                            }),
                        );
                    }

                    Node::Delta0(delta) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Delta0(Delta0 {
                                input: delta.input(),
                            }),
                        );
                    }

                    Node::DelayedFeedback(_) => {
                        nodes.insert(*node_id, DataflowNode::DelayedFeedback(DelayedFeedback {}));
                    }

                    Node::Min(min) => {
                        nodes.insert(*node_id, DataflowNode::Min(Min { input: min.input() }));
                    }

                    Node::Max(max) => {
                        nodes.insert(*node_id, DataflowNode::Max(Max { input: max.input() }));
                    }

                    Node::Distinct(distinct) => {
                        nodes.insert(
                            *node_id,
                            DataflowNode::Distinct(Distinct {
                                input: distinct.input(),
                            }),
                        );
                    }

                    Node::JoinCore(join) => {
                        let (lhs, rhs) = (join.lhs(), join.rhs());
                        let join_fn = jit.get_finalized_function(node_functions[node_id][0]);
                        let key_vtable = unsafe { &*vtables[&join.key_layout()] };
                        let value_vtable = unsafe { &*vtables[&join.value_layout()] };

                        let node = match (node_streams[&lhs].unwrap(), node_streams[&rhs].unwrap())
                        {
                            (StreamLayout::Map(..), StreamLayout::Map(..)) => {
                                DataflowNode::JoinCore(JoinCore {
                                    lhs,
                                    rhs,
                                    join_fn: unsafe {
                                        transmute::<
                                            _,
                                            unsafe extern "C" fn(
                                                *const u8,
                                                *const u8,
                                                *const u8,
                                                *mut u8,
                                                *mut u8,
                                            ),
                                        >(join_fn)
                                    },
                                    key_vtable,
                                    value_vtable,
                                    output_kind: join.result_kind(),
                                })
                            }

                            _ => todo!(),
                        };

                        nodes.insert(*node_id, node);
                    }

                    Node::MonotonicJoin(join) => {
                        let (lhs, rhs) = (join.lhs(), join.rhs());
                        let join_fn = jit.get_finalized_function(node_functions[node_id][0]);
                        let key_vtable = unsafe { &*vtables[&join.key_layout()] };

                        let node = DataflowNode::MonotonicJoin(MonotonicJoin {
                            lhs,
                            rhs,
                            join_fn: unsafe {
                                transmute::<
                                    _,
                                    unsafe extern "C" fn(*const u8, *const u8, *const u8, *mut u8),
                                >(join_fn)
                            },
                            key_vtable,
                        });
                        nodes.insert(*node_id, node);
                    }

                    Node::Antijoin(antijoin) => {
                        let node = DataflowNode::Antijoin(Antijoin {
                            lhs: antijoin.lhs(),
                            rhs: antijoin.rhs(),
                        });
                        nodes.insert(*node_id, node);
                    }

                    Node::Subgraph(subgraph) => {
                        let node = DataflowNode::Subgraph(DataflowSubgraph {
                            edges: subgraph.edges().clone(),
                            inputs: subgraph.input_nodes().clone(),
                            nodes: compile_nodes(
                                subgraph.subgraph(),
                                vtables,
                                jit,
                                node_streams,
                                node_functions,
                                layout_cache,
                            ),
                            feedback_connections: subgraph.feedback_connections().clone(),
                        });
                        nodes.insert(*node_id, node);
                    }

                    Node::Export(export) => {
                        let node = DataflowNode::Export(Export {
                            input: export.input(),
                        });
                        nodes.insert(*node_id, node);
                    }

                    Node::ExportedNode(exported) => {
                        let node = DataflowNode::Noop(Noop {
                            input: exported.input(),
                        });
                        nodes.insert(*node_id, node);
                    }

                    Node::Constant(constant) => {
                        let value = match constant.value().value() {
                            StreamCollection::Set(set) => {
                                let key_layout = constant.layout().unwrap_set();
                                let key_vtable = unsafe { &*vtables[&key_layout] };
                                let key_layout = layout_cache.layout_of(key_layout);

                                let mut batch = Vec::with_capacity(set.len());
                                for (literal, diff) in set {
                                    let key = unsafe {
                                        row_from_literal(literal, key_vtable, &key_layout)
                                    };

                                    batch.push((key, *diff));
                                }

                                // Build a batch from the set's values
                                let mut batcher = <RowSet as Batch>::Batcher::new_batcher(());
                                batcher.push_batch(&mut batch);
                                RowZSet::Set(batcher.seal())
                            }

                            StreamCollection::Map(map) => {
                                let (key_layout, value_layout) = constant.layout().unwrap_map();
                                let (key_vtable, value_vtable) =
                                    unsafe { (&*vtables[&key_layout], &*vtables[&value_layout]) };
                                let (key_layout, value_layout) = (
                                    layout_cache.layout_of(key_layout),
                                    layout_cache.layout_of(value_layout),
                                );

                                let mut batch = Vec::with_capacity(map.len());
                                for (key_literal, value_literal, diff) in map {
                                    let key = unsafe {
                                        row_from_literal(key_literal, key_vtable, &key_layout)
                                    };
                                    let value = unsafe {
                                        row_from_literal(value_literal, value_vtable, &value_layout)
                                    };

                                    batch.push(((key, value), *diff));
                                }

                                // Build a batch from the set's values
                                let mut batcher = <RowMap as Batch>::Batcher::new_batcher(());
                                batcher.push_batch(&mut batch);
                                RowZSet::Map(batcher.seal())
                            }
                        };

                        let node = DataflowNode::Constant(nodes::Constant { value });
                        nodes.insert(*node_id, node);
                    }
                }
            }

            nodes
        }

        let (jit, native_layout_cache) = codegen.finalize_definitions();
        let vtables = vtables
            .into_iter()
            .map(|(layout, vtable)| (layout, Box::into_raw(Box::new(vtable.marshalled(&jit)))))
            .collect();
        let nodes = compile_nodes(
            graph.graph(),
            &vtables,
            &jit,
            &node_streams,
            &node_functions,
            &native_layout_cache,
        );

        (
            Self {
                nodes,
                edges: graph.edges().clone(),
            },
            JitHandle { jit, vtables },
            native_layout_cache,
        )
    }

    pub fn construct(mut self, circuit: &mut RootCircuit) -> (Inputs, Outputs) {
        let mut streams = BTreeMap::<NodeId, RowStream<RootCircuit>>::new();

        let mut inputs = BTreeMap::new();
        let mut outputs = BTreeMap::new();

        let order = algo::toposort(&self.edges, None).unwrap();
        for node_id in order {
            let node = match self.nodes.remove(&node_id) {
                Some(node) => node,
                None => continue,
            };

            match node {
                DataflowNode::Map(map) => self.map(node_id, map, &mut streams),
                DataflowNode::Filter(filter) => self.filter(node_id, filter, &mut streams),

                DataflowNode::FilterMap(map) => {
                    let input = &streams[&map.input];
                    let mapped = match input {
                        RowStream::Set(input) => {
                            let (fmap_fn, vtable) = (map.filter_map, map.output_vtable);

                            let mut output = None;
                            RowStream::Set(input.flat_map(move |input| {
                                let mut out =
                                    output.take().unwrap_or_else(|| UninitRow::new(vtable));

                                if unsafe { fmap_fn(input.as_ptr(), out.as_mut_ptr()) } {
                                    Some(unsafe { out.assume_init() })
                                } else {
                                    output = Some(out);
                                    None
                                }
                            }))
                        }

                        RowStream::Map(_) => todo!(),
                    };

                    streams.insert(node_id, mapped);
                }

                DataflowNode::FilterMapIndex(map) => {
                    let input = &streams[&map.input];
                    let mapped = match input {
                        RowStream::Map(input) => {
                            let (fmap_fn, vtable) = (map.filter_map, map.output_vtable);

                            let mut output = None;
                            RowStream::Set(input.flat_map(move |(key, value)| {
                                let mut out =
                                    output.take().unwrap_or_else(|| UninitRow::new(vtable));

                                let keep_element = unsafe {
                                    fmap_fn(key.as_ptr(), value.as_ptr(), out.as_mut_ptr())
                                };
                                if keep_element {
                                    Some(unsafe { out.assume_init() })
                                } else {
                                    output = Some(out);
                                    None
                                }
                            }))
                        }

                        RowStream::Set(_) => todo!(),
                    };

                    streams.insert(node_id, mapped);
                }

                DataflowNode::FlatMap(flat_map) => self.flat_map(node_id, flat_map, &mut streams),

                DataflowNode::IndexWith(index_with) => {
                    let input = &streams[&index_with.input];
                    let (index_fn, key_vtable, value_vtable) = (
                        index_with.index_fn,
                        index_with.key_vtable,
                        index_with.value_vtable,
                    );

                    let indexed = match input {
                        RowStream::Set(input) => input.index_with(move |input| {
                            let (mut key_output, mut value_output) =
                                (UninitRow::new(key_vtable), UninitRow::new(value_vtable));

                            unsafe {
                                index_fn(
                                    input.as_ptr(),
                                    key_output.as_mut_ptr(),
                                    value_output.as_mut_ptr(),
                                );

                                (key_output.assume_init(), value_output.assume_init())
                            }
                        }),

                        // FIXME: `.index_with()` requires that `Key` is a `()`
                        RowStream::Map(_) => todo!(),
                    };

                    streams.insert(node_id, RowStream::Map(indexed));
                }

                DataflowNode::Sum(sum) => {
                    let sum = match &streams[&sum.inputs[0]] {
                        RowStream::Set(first) => {
                            RowStream::Set(first.sum(sum.inputs[1..].iter().map(|input| {
                                if let RowStream::Set(input) = &streams[input] {
                                    input
                                } else {
                                    unreachable!()
                                }
                            })))
                        }
                        RowStream::Map(first) => {
                            RowStream::Map(first.sum(sum.inputs[1..].iter().map(|input| {
                                if let RowStream::Map(input) = &streams[input] {
                                    input
                                } else {
                                    unreachable!()
                                }
                            })))
                        }
                    };
                    streams.insert(node_id, sum);
                }

                DataflowNode::Minus(minus) => {
                    let lhs = &streams[&minus.lhs];
                    let rhs = streams[&minus.rhs].clone();
                    let difference = match lhs {
                        RowStream::Set(first) => RowStream::Set(first.minus(&rhs.unwrap_set())),
                        RowStream::Map(first) => RowStream::Map(first.minus(&rhs.unwrap_map())),
                    };
                    streams.insert(node_id, difference);
                }

                DataflowNode::Neg(neg) => self.neg(node_id, neg, &mut streams),

                DataflowNode::Differentiate(diff) => {
                    let input = &streams[&diff.input];
                    let differentiated = match input {
                        RowStream::Set(input) => RowStream::Set(input.differentiate()),
                        RowStream::Map(input) => RowStream::Map(input.differentiate()),
                    };

                    streams.insert(node_id, differentiated);
                }

                DataflowNode::Integrate(integrate) => {
                    let input = &streams[&integrate.input];
                    // TODO: It'd be better if we used `.integrate_trace()`
                    let integrated = match input {
                        RowStream::Set(input) => RowStream::Set(input.integrate()),
                        RowStream::Map(input) => RowStream::Map(input.integrate()),
                    };

                    streams.insert(node_id, integrated);
                }

                DataflowNode::Sink(sink) => {
                    let input = &streams[&sink.input];
                    let output = match input {
                        RowStream::Set(input) => RowOutput::Set(input.output()),
                        RowStream::Map(input) => RowOutput::Map(input.output()),
                    };

                    outputs.insert(node_id, output);
                }

                DataflowNode::Source(source) => {
                    let (stream, handle) = circuit.add_input_zset::<Row, i32>();

                    if cfg!(debug_assertions) {
                        stream.inspect(|row| {
                            let mut cursor = row.cursor();
                            while cursor.key_valid() {
                                let key = cursor.key();
                                assert_eq!(key.vtable().layout_id, source.output_vtable.layout_id);
                                cursor.step_key();
                            }
                        });
                    }

                    streams.insert(node_id, RowStream::Set(stream));
                    inputs.insert(node_id, RowInput::Set(handle));
                }

                DataflowNode::SourceMap(_source) => {
                    let (stream, handle) = circuit.add_input_indexed_zset::<Row, Row, i32>();
                    streams.insert(node_id, RowStream::Map(stream));
                    inputs.insert(node_id, RowInput::Map(handle));
                }

                DataflowNode::Delta0(_) => todo!(),

                DataflowNode::DelayedFeedback(_) => todo!(),

                DataflowNode::Min(min) => {
                    let min = match &streams[&min.input] {
                        RowStream::Set(_) => todo!(),
                        RowStream::Map(input) => {
                            RowStream::Map(input.aggregate_generic(dbsp::operator::Min))
                        }
                    };
                    streams.insert(node_id, min);
                }

                DataflowNode::Max(max) => {
                    let max = match &streams[&max.input] {
                        RowStream::Set(_) => todo!(),
                        RowStream::Map(input) => {
                            RowStream::Map(input.aggregate_generic(dbsp::operator::Max))
                        }
                    };
                    streams.insert(node_id, max);
                }

                DataflowNode::Fold(fold) => {
                    let (step_fn, finish_fn) = (fold.step_fn, fold.finish_fn);
                    let (acc_vtable, step_vtable, output_vtable) =
                        (fold.acc_vtable, fold.step_vtable, fold.output_vtable);

                    let folded = match &streams[&fold.input] {
                        RowStream::Set(_) => todo!(),

                        RowStream::Map(input) => input.aggregate(dbsp::operator::Fold::<
                            _,
                            UnimplementedSemigroup<Row>,
                            _,
                            _,
                        >::with_output(
                            fold.init,
                            move |acc: &mut Row, step: &Row, weight| unsafe {
                                debug_assert_eq!(acc.vtable().layout_id, acc_vtable.layout_id);
                                debug_assert_eq!(step.vtable().layout_id, step_vtable.layout_id);

                                step_fn(
                                    acc.as_mut_ptr(),
                                    step.as_ptr(),
                                    &weight as *const i32 as *const u8,
                                );
                            },
                            move |mut acc: Row| unsafe {
                                debug_assert_eq!(acc.vtable().layout_id, acc_vtable.layout_id);

                                let mut row = UninitRow::new(output_vtable);
                                finish_fn(acc.as_mut_ptr(), row.as_mut_ptr());
                                row.assume_init()
                            },
                        )),
                    };
                    streams.insert(node_id, RowStream::Map(folded));
                }

                DataflowNode::PartitionedRollingFold(_fold) => {
                    /*
                    let (step_fn, finish_fn) = (fold.step_fn, fold.finish_fn);
                    let (acc_vtable, step_vtable, output_vtable) =
                        (fold.acc_vtable, fold.step_vtable, fold.output_vtable);

                    let folded = match &streams[&fold.input] {
                        RowStream::Set(_) => todo!(),

                        RowStream::Map(input) => {
                            let fold_agg = dbsp::operator::Fold::<
                                _,
                                UnimplementedSemigroup<_>,
                                _,
                                _,
                            >::with_output(
                                fold.init,
                                move |acc: &mut Row, step: &Row, weight| unsafe {
                                    debug_assert_eq!(acc.vtable().layout_id, acc_vtable.layout_id);
                                    debug_assert_eq!(
                                        step.vtable().layout_id,
                                        step_vtable.layout_id
                                    );

                                    step_fn(
                                        acc.as_mut_ptr(),
                                        step.as_ptr(),
                                        &weight as *const i32 as *const u8,
                                    );
                                },
                                move |mut acc: Row| unsafe {
                                    debug_assert_eq!(acc.vtable().layout_id, acc_vtable.layout_id);

                                    let mut row = UninitRow::new(output_vtable);
                                    finish_fn(acc.as_mut_ptr(), row.as_mut_ptr());
                                    row.assume_init()
                                },
                            );

                            input.partitioned_rolling_aggregate(fold_agg, fold.range)
                        }
                    };
                    streams.insert(node_id, RowStream::Map(folded));
                    */
                    unimplemented!()
                }

                DataflowNode::Distinct(distinct) => self.distinct(node_id, distinct, &mut streams),

                DataflowNode::JoinCore(join) => {
                    let lhs = streams[&join.lhs].clone();
                    let rhs = streams[&join.rhs].clone();
                    let (join_fn, key_vtable, _value_vtable) =
                        (join.join_fn, join.key_vtable, join.value_vtable);

                    let joined = match join.output_kind {
                        StreamKind::Set => RowStream::Set(lhs.unwrap_map().join_generic(
                            &rhs.unwrap_map(),
                            move |key, lhs_val, rhs_val| {
                                let mut output = UninitRow::new(key_vtable);
                                unsafe {
                                    join_fn(
                                        key.as_ptr(),
                                        lhs_val.as_ptr(),
                                        rhs_val.as_ptr(),
                                        output.as_mut_ptr(),
                                        NonNull::<u8>::dangling().as_ptr(),
                                    );
                                }

                                iter::once((unsafe { output.assume_init() }, ()))
                            },
                        )),

                        StreamKind::Map => todo!(),
                    };
                    streams.insert(node_id, joined);
                }

                DataflowNode::MonotonicJoin(join) => {
                    let lhs = &streams[&join.lhs];
                    let rhs = streams[&join.rhs].clone();
                    let (join_fn, key_vtable) = (join.join_fn, join.key_vtable);

                    let joined = match lhs {
                        RowStream::Set(lhs) => {
                            RowStream::Set(lhs.monotonic_stream_join::<_, _, _>(
                                &rhs.unwrap_set(),
                                move |key, lhs_val, rhs_val| {
                                    let mut output = UninitRow::new(key_vtable);
                                    unsafe {
                                        join_fn(
                                            key.as_ptr(),
                                            lhs_val as *const () as *const u8,
                                            rhs_val as *const () as *const u8,
                                            output.as_mut_ptr(),
                                        );

                                        output.assume_init()
                                    }
                                },
                            ))
                        }

                        RowStream::Map(lhs) => {
                            RowStream::Set(lhs.monotonic_stream_join::<_, _, _>(
                                &rhs.unwrap_map(),
                                move |key, lhs_val, rhs_val| {
                                    let mut output = UninitRow::new(key_vtable);
                                    unsafe {
                                        join_fn(
                                            key.as_ptr(),
                                            lhs_val.as_ptr(),
                                            rhs_val.as_ptr(),
                                            output.as_mut_ptr(),
                                        );

                                        output.assume_init()
                                    }
                                },
                            ))
                        }
                    };
                    streams.insert(node_id, joined);
                }

                DataflowNode::Antijoin(antijoin) => self.antijoin(node_id, antijoin, &mut streams),

                DataflowNode::Export(_) => todo!(),

                DataflowNode::Constant(constant) => {
                    self.constant(node_id, constant, circuit, &mut streams);
                }

                DataflowNode::Subgraph(subgraph) => self.subgraph(subgraph, circuit, &mut streams),

                DataflowNode::Noop(_) => {}
            }
        }

        (inputs, outputs)
    }

    fn subgraph(
        &mut self,
        mut subgraph: DataflowSubgraph,
        circuit: &mut RootCircuit,
        streams: &mut BTreeMap<NodeId, RowStream<RootCircuit>>,
    ) {
        let mut needs_consolidate = BTreeMap::new();

        circuit
            .fixedpoint(|subcircuit| {
                let mut substreams = BTreeMap::new();
                let mut feedbacks = BTreeMap::new();

                let nodes = algo::toposort(&subgraph.edges, None).unwrap();
                for node_id in nodes {
                    if subgraph.inputs.contains_key(&node_id) {
                        continue;
                    }

                    let node = match subgraph.nodes.remove(&node_id) {
                        Some(node) => node,
                        None => continue,
                    };
                    match node {
                        DataflowNode::Constant(constant) => {
                            self.constant(node_id, constant, subcircuit, &mut substreams);
                        }
                        DataflowNode::Map(map) => self.map(node_id, map, &mut substreams),
                        DataflowNode::Filter(filter) => {
                            self.filter(node_id, filter, &mut substreams);
                        }

                        DataflowNode::FilterMap(map) => {
                            let input = &substreams[&map.input];
                            let mapped = match input {
                                RowStream::Set(input) => {
                                    let (fmap_fn, vtable) = (map.filter_map, map.output_vtable);

                                    let mut output = None;
                                    RowStream::Set(input.flat_map(move |input| {
                                        let mut out =
                                            output.take().unwrap_or_else(|| UninitRow::new(vtable));
                                        unsafe {
                                            if fmap_fn(input.as_ptr(), out.as_mut_ptr()) {
                                                Some(out.assume_init())
                                            } else {
                                                output = Some(out);
                                                None
                                            }
                                        }
                                    }))
                                }

                                RowStream::Map(_) => todo!(),
                            };

                            substreams.insert(node_id, mapped);
                        }

                        DataflowNode::FilterMapIndex(map) => {
                            let input = &substreams[&map.input];
                            let mapped = match input {
                                RowStream::Map(input) => {
                                    let (fmap_fn, vtable) = (map.filter_map, map.output_vtable);

                                    let mut output = None;
                                    RowStream::Set(input.flat_map(move |(key, value)| {
                                        let mut out =
                                            output.take().unwrap_or_else(|| UninitRow::new(vtable));

                                        unsafe {
                                            if fmap_fn(
                                                key.as_ptr(),
                                                value.as_ptr(),
                                                out.as_mut_ptr(),
                                            ) {
                                                Some(out.assume_init())
                                            } else {
                                                output = Some(out);
                                                None
                                            }
                                        }
                                    }))
                                }

                                RowStream::Set(_) => todo!(),
                            };

                            substreams.insert(node_id, mapped);
                        }

                        DataflowNode::FlatMap(flat_map) => {
                            self.flat_map(node_id, flat_map, &mut substreams);
                        }

                        DataflowNode::IndexWith(index_with) => {
                            let input = &substreams[&index_with.input];
                            let (index_fn, key_vtable, value_vtable) = (
                                index_with.index_fn,
                                index_with.key_vtable,
                                index_with.value_vtable,
                            );

                            let indexed = match input {
                                RowStream::Set(input) => input.index_with(move |input| {
                                    let (mut key_output, mut value_output) =
                                        (UninitRow::new(key_vtable), UninitRow::new(value_vtable));

                                    unsafe {
                                        index_fn(
                                            input.as_ptr(),
                                            key_output.as_mut_ptr(),
                                            value_output.as_mut_ptr(),
                                        );

                                        (key_output.assume_init(), value_output.assume_init())
                                    }
                                }),

                                // FIXME: `.index_with()` requires that `Key` is a `()`
                                RowStream::Map(_) => todo!(),
                            };

                            substreams.insert(node_id, RowStream::Map(indexed));
                        }

                        DataflowNode::Sum(sum) => {
                            let sum = match &substreams[&sum.inputs[0]] {
                                RowStream::Set(first) => {
                                    RowStream::Set(first.sum(sum.inputs[1..].iter().map(|input| {
                                        if let RowStream::Set(input) = &substreams[input] {
                                            input
                                        } else {
                                            unreachable!()
                                        }
                                    })))
                                }
                                RowStream::Map(first) => {
                                    RowStream::Map(first.sum(sum.inputs[1..].iter().map(|input| {
                                        if let RowStream::Map(input) = &substreams[input] {
                                            input
                                        } else {
                                            unreachable!()
                                        }
                                    })))
                                }
                            };
                            substreams.insert(node_id, sum);
                        }

                        DataflowNode::Minus(minus) => {
                            let lhs = &substreams[&minus.lhs];
                            let rhs = substreams[&minus.rhs].clone();
                            let difference = match lhs {
                                RowStream::Set(first) => {
                                    RowStream::Set(first.minus(&rhs.unwrap_set()))
                                }
                                RowStream::Map(first) => {
                                    RowStream::Map(first.minus(&rhs.unwrap_map()))
                                }
                            };
                            substreams.insert(node_id, difference);
                        }

                        DataflowNode::Neg(neg) => self.neg(node_id, neg, &mut substreams),

                        DataflowNode::Sink(_)
                        | DataflowNode::Source(_)
                        | DataflowNode::SourceMap(_) => todo!(),

                        DataflowNode::Delta0(delta) => {
                            let input = &streams[&delta.input];
                            let delta0 = match input {
                                RowStream::Set(input) => RowStream::Set(input.delta0(subcircuit)),
                                RowStream::Map(input) => RowStream::Map(input.delta0(subcircuit)),
                            };

                            substreams.insert(node_id, delta0);
                        }

                        DataflowNode::DelayedFeedback(_feedback) => {
                            let feedback =
                                dbsp::operator::DelayedFeedback::<_, RowSet>::new(subcircuit);
                            let stream = feedback.stream().clone();
                            substreams.insert(node_id, RowStream::Set(stream));
                            feedbacks.insert(node_id, feedback);
                        }

                        DataflowNode::Min(min) => {
                            let min = match &substreams[&min.input] {
                                RowStream::Set(_) => todo!(),
                                RowStream::Map(input) => {
                                    RowStream::Map(input.aggregate_generic(dbsp::operator::Min))
                                }
                            };
                            substreams.insert(node_id, min);
                        }

                        DataflowNode::Max(max) => {
                            let max = match &substreams[&max.input] {
                                RowStream::Set(_) => todo!(),
                                RowStream::Map(input) => {
                                    RowStream::Map(input.aggregate_generic(dbsp::operator::Max))
                                }
                            };
                            substreams.insert(node_id, max);
                        }

                        DataflowNode::Fold(fold) => {
                            let (step_fn, finish_fn) = (fold.step_fn, fold.finish_fn);
                            let (acc_vtable, step_vtable, output_vtable) =
                                (fold.acc_vtable, fold.step_vtable, fold.output_vtable);

                            let folded = match &substreams[&fold.input] {
                                RowStream::Set(_) => todo!(),

                                RowStream::Map(input) => input.aggregate(dbsp::operator::Fold::<
                                    _,
                                    UnimplementedSemigroup<Row>,
                                    _,
                                    _,
                                >::with_output(
                                    fold.init,
                                    move |acc: &mut Row, step: &Row, weight| unsafe {
                                        debug_assert_eq!(
                                            acc.vtable().layout_id,
                                            acc_vtable.layout_id
                                        );
                                        debug_assert_eq!(
                                            step.vtable().layout_id,
                                            step_vtable.layout_id
                                        );

                                        step_fn(
                                            acc.as_mut_ptr(),
                                            step.as_ptr(),
                                            &weight as *const i32 as *const u8,
                                        );
                                    },
                                    move |mut acc: Row| unsafe {
                                        debug_assert_eq!(
                                            acc.vtable().layout_id,
                                            acc_vtable.layout_id
                                        );

                                        let mut row = UninitRow::new(output_vtable);
                                        finish_fn(acc.as_mut_ptr(), row.as_mut_ptr());
                                        row.assume_init()
                                    },
                                )),
                            };
                            substreams.insert(node_id, RowStream::Map(folded));
                        }

                        DataflowNode::PartitionedRollingFold(_fold) => {
                            /*
                            let (step_fn, finish_fn) = (fold.step_fn, fold.finish_fn);
                            let (acc_vtable, step_vtable, output_vtable) =
                                (fold.acc_vtable, fold.step_vtable, fold.output_vtable);

                            let folded = match &streams[&fold.input] {
                                RowStream::Set(_) => todo!(),

                                RowStream::Map(input) => {
                                    let fold_agg = dbsp::operator::Fold::<
                                        _,
                                        UnimplementedSemigroup<_>,
                                        _,
                                        _,
                                    >::with_output(
                                        fold.init,
                                        move |acc: &mut Row, step: &Row, weight| unsafe {
                                            debug_assert_eq!(acc.vtable().layout_id, acc_vtable.layout_id);
                                            debug_assert_eq!(
                                                step.vtable().layout_id,
                                                step_vtable.layout_id
                                            );

                                            step_fn(
                                                acc.as_mut_ptr(),
                                                step.as_ptr(),
                                                &weight as *const i32 as *const u8,
                                            );
                                        },
                                        move |mut acc: Row| unsafe {
                                            debug_assert_eq!(acc.vtable().layout_id, acc_vtable.layout_id);

                                            let mut row = UninitRow::new(output_vtable);
                                            finish_fn(acc.as_mut_ptr(), row.as_mut_ptr());
                                            (0, row.assume_init())
                                        },
                                    );

                                    input.partitioned_rolling_aggregate::<i32, Row, _>(fold_agg, fold.range)
                                }
                            };
                            streams.insert(node_id, RowStream::Map(folded));
                             */
                            unimplemented!()
                        }

                        DataflowNode::Distinct(distinct) => {
                            self.distinct(node_id, distinct, &mut substreams);
                        }

                        DataflowNode::JoinCore(join) => {
                            let lhs = substreams[&join.lhs].clone();
                            let rhs = substreams[&join.rhs].clone();
                            let (join_fn, key_vtable, _value_vtable) =
                                (join.join_fn, join.key_vtable, join.value_vtable);

                            let joined = match join.output_kind {
                                StreamKind::Set => RowStream::Set(lhs.unwrap_map().join_generic(
                                    &rhs.unwrap_map(),
                                    move |key, lhs_val, rhs_val| {
                                        let mut output = UninitRow::new(key_vtable);
                                        unsafe {
                                            join_fn(
                                                key.as_ptr(),
                                                lhs_val.as_ptr(),
                                                rhs_val.as_ptr(),
                                                output.as_mut_ptr(),
                                                NonNull::<u8>::dangling().as_ptr(),
                                            );
                                        }

                                        iter::once((unsafe { output.assume_init() }, ()))
                                    },
                                )),

                                StreamKind::Map => todo!(),
                            };
                            substreams.insert(node_id, joined);
                        }

                        DataflowNode::MonotonicJoin(join) => {
                            let lhs = &substreams[&join.lhs];
                            let rhs = substreams[&join.rhs].clone();
                            let (join_fn, key_vtable) = (join.join_fn, join.key_vtable);

                            let joined = match lhs {
                                RowStream::Set(lhs) => {
                                    RowStream::Set(lhs.monotonic_stream_join::<_, _, _>(
                                        &rhs.unwrap_set(),
                                        move |key, lhs_val, rhs_val| {
                                            let mut output = UninitRow::new(key_vtable);
                                            unsafe {
                                                join_fn(
                                                    key.as_ptr(),
                                                    lhs_val as *const () as *const u8,
                                                    rhs_val as *const () as *const u8,
                                                    output.as_mut_ptr(),
                                                );

                                                output.assume_init()
                                            }
                                        },
                                    ))
                                }

                                RowStream::Map(lhs) => {
                                    RowStream::Set(lhs.monotonic_stream_join::<_, _, _>(
                                        &rhs.unwrap_map(),
                                        move |key, lhs_val, rhs_val| {
                                            let mut output = UninitRow::new(key_vtable);
                                            unsafe {
                                                join_fn(
                                                    key.as_ptr(),
                                                    lhs_val.as_ptr(),
                                                    rhs_val.as_ptr(),
                                                    output.as_mut_ptr(),
                                                );

                                                output.assume_init()
                                            }
                                        },
                                    ))
                                }
                            };
                            substreams.insert(node_id, joined);
                        }

                        DataflowNode::Antijoin(antijoin) => {
                            self.antijoin(node_id, antijoin, &mut substreams)
                        }

                        DataflowNode::Export(export) => {
                            let exported = match &substreams[&export.input] {
                                RowStream::Set(input) => {
                                    RowTrace::Set(input.integrate_trace().export())
                                }
                                RowStream::Map(input) => {
                                    RowTrace::Map(input.integrate_trace().export())
                                }
                            };

                            needs_consolidate.insert(node_id, exported);
                        }

                        DataflowNode::Differentiate(diff) => {
                            let input = &streams[&diff.input];
                            let differentiated = match input {
                                RowStream::Set(input) => {
                                    RowStream::Set(input.differentiate_nested())
                                }
                                RowStream::Map(input) => {
                                    RowStream::Map(input.differentiate_nested())
                                }
                            };

                            streams.insert(node_id, differentiated);
                        }

                        DataflowNode::Integrate(diff) => {
                            let input = &streams[&diff.input];
                            let integrated = match input {
                                RowStream::Set(input) => RowStream::Set(input.integrate_nested()),
                                RowStream::Map(input) => RowStream::Map(input.integrate_nested()),
                            };

                            streams.insert(node_id, integrated);
                        }

                        DataflowNode::Subgraph(_) => todo!(),

                        DataflowNode::Noop(_) => {}
                    }
                }

                // Connect all feedback nodes
                for (source, feedback) in subgraph.feedback_connections.iter() {
                    let source = substreams[source].clone().unwrap_set();
                    let feedback = feedbacks.remove(feedback).unwrap();
                    feedback.connect(&source);
                }

                Ok(())
            })
            .unwrap();

        for (node_id, stream) in needs_consolidate {
            let consolidated = match stream {
                RowTrace::Set(stream) => RowStream::Set(stream.consolidate()),
                RowTrace::Map(stream) => RowStream::Map(stream.consolidate()),
            };
            streams.insert(node_id, consolidated);
        }
    }

    fn distinct<C>(
        &mut self,
        node_id: NodeId,
        distinct: Distinct,
        streams: &mut BTreeMap<NodeId, RowStream<C>>,
    ) where
        C: Circuit,
        C::Time: DBTimestamp,
    {
        let distinct = match &streams[&distinct.input] {
            RowStream::Set(input) => RowStream::Set(input.distinct()),
            RowStream::Map(input) => RowStream::Map(input.distinct()),
        };
        streams.insert(node_id, distinct);
    }

    fn flat_map<C>(
        &mut self,
        node_id: NodeId,
        flat_map: FlatMap,
        streams: &mut BTreeMap<NodeId, RowStream<C>>,
    ) where
        C: Circuit,
    {
        let input = &streams[&flat_map.input];
        let mapped = match flat_map.flat_map {
            FlatMapFn::SetSet {
                flat_map,
                key_vtable,
            } => {
                let input = input.as_set().unwrap();
                let flat_map =
                    operators::FlatMap::new(move |key: &Row, output_keys: &mut Vec<Row>| unsafe {
                        flat_map(
                            key.as_ptr(),
                            &mut [
                                output_keys as *mut _ as *mut u8,
                                key_vtable as *const _ as *mut u8,
                            ],
                        )
                    });
                RowStream::Set(input.circuit().add_unary_operator(flat_map, input))
            }

            FlatMapFn::SetMap {
                flat_map,
                key_vtable,
                value_vtable,
            } => {
                let input = input.as_set().unwrap();
                let flat_map = operators::FlatMap::new(
                    move |key: &Row, output_keys: &mut Vec<Row>, output_values: &mut Vec<Row>| unsafe {
                        flat_map(
                            key.as_ptr(),
                            &mut [
                                output_keys as *mut _ as *mut u8,
                                key_vtable as *const _ as *mut u8,
                            ],
                            &mut [
                                output_values as *mut _ as *mut u8,
                                value_vtable as *const _ as *mut u8,
                            ],
                        )
                    },
                );
                RowStream::Map(input.circuit().add_unary_operator(flat_map, input))
            }

            FlatMapFn::MapSet {
                flat_map,
                key_vtable,
            } => {
                let input = input.as_map().unwrap();
                let flat_map = operators::FlatMap::new(
                    move |key: &Row, value: &Row, output_keys: &mut Vec<Row>| unsafe {
                        flat_map(
                            key.as_ptr(),
                            value.as_ptr(),
                            &mut [
                                output_keys as *mut _ as *mut u8,
                                key_vtable as *const _ as *mut u8,
                            ],
                        )
                    },
                );
                RowStream::Set(input.circuit().add_unary_operator(flat_map, input))
            }

            FlatMapFn::MapMap {
                flat_map,
                key_vtable,
                value_vtable,
            } => {
                let input = input.as_map().unwrap();
                let flat_map = operators::FlatMap::new(
                    move |key: &Row,
                          value: &Row,
                          output_keys: &mut Vec<Row>,
                          output_values: &mut Vec<Row>| unsafe {
                        flat_map(
                            key.as_ptr(),
                            value.as_ptr(),
                            &mut [
                                output_keys as *mut _ as *mut u8,
                                key_vtable as *const _ as *mut u8,
                            ],
                            &mut [
                                output_values as *mut _ as *mut u8,
                                value_vtable as *const _ as *mut u8,
                            ],
                        )
                    },
                );
                RowStream::Map(input.circuit().add_unary_operator(flat_map, input))
            }
        };

        streams.insert(node_id, mapped);
    }

    fn antijoin<C>(
        &self,
        node_id: NodeId,
        antijoin: Antijoin,
        streams: &mut BTreeMap<NodeId, RowStream<C>>,
    ) where
        C: Circuit,
        C::Time: DBTimestamp,
    {
        let antijoined = match (&streams[&antijoin.lhs], &streams[&antijoin.rhs]) {
            (RowStream::Set(lhs), RowStream::Set(rhs)) => RowStream::Set(lhs.antijoin(rhs)),
            (RowStream::Set(lhs), RowStream::Map(rhs)) => RowStream::Set(lhs.antijoin(rhs)),
            (RowStream::Map(lhs), RowStream::Set(rhs)) => RowStream::Map(lhs.antijoin(rhs)),
            (RowStream::Map(lhs), RowStream::Map(rhs)) => RowStream::Map(lhs.antijoin(rhs)),
        };

        streams.insert(node_id, antijoined);
    }

    fn neg<C>(&self, node_id: NodeId, neg: Neg, streams: &mut BTreeMap<NodeId, RowStream<C>>)
    where
        C: Circuit,
    {
        let negated = match &streams[&neg.input] {
            RowStream::Set(input) => RowStream::Set(input.neg()),
            RowStream::Map(input) => RowStream::Map(input.neg()),
        };

        streams.insert(node_id, negated);
    }

    fn filter<C>(
        &self,
        node_id: NodeId,
        filter: Filter,
        streams: &mut BTreeMap<NodeId, RowStream<C>>,
    ) where
        C: Circuit,
    {
        let filtered = match (filter.filter_fn, &streams[&filter.input()]) {
            (FilterFn::Set(filter_fn), RowStream::Set(input)) => {
                let filtered = input.filter(move |input| unsafe { filter_fn(input.as_ptr()) });
                RowStream::Set(filtered)
            }

            (FilterFn::Map(filter_fn), RowStream::Map(input)) => {
                let filtered = input
                    .filter(move |(key, value)| unsafe { filter_fn(key.as_ptr(), value.as_ptr()) });
                RowStream::Map(filtered)
            }

            _ => unreachable!(),
        };

        streams.insert(node_id, filtered);
    }

    fn constant<C>(
        &self,
        node_id: NodeId,
        constant: nodes::Constant,
        circuit: &mut C,
        streams: &mut BTreeMap<NodeId, RowStream<C>>,
    ) where
        C: Circuit,
    {
        let constant = match constant.value {
            RowZSet::Set(set) => {
                RowStream::Set(circuit.add_source(Generator::new(move || set.clone())))
            }

            RowZSet::Map(map) => {
                RowStream::Map(circuit.add_source(Generator::new(move || map.clone())))
            }
        };
        streams.insert(node_id, constant);
    }

    fn map<C>(&self, node_id: NodeId, map: Map, streams: &mut BTreeMap<NodeId, RowStream<C>>)
    where
        C: Circuit,
    {
        let input = streams[&map.input].clone();

        let mapped = match map.map_fn {
            MapFn::SetSet { map, key_vtable } => {
                RowStream::Set(input.unwrap_set().map(move |input| {
                    let mut output = UninitRow::new(key_vtable);
                    unsafe {
                        map(input.as_ptr(), output.as_mut_ptr());
                        output.assume_init()
                    }
                }))
            }

            MapFn::SetMap {
                map,
                key_vtable,
                value_vtable,
            } => RowStream::Map(input.unwrap_set().map_index(move |input| {
                let (mut key_output, mut value_output) =
                    (UninitRow::new(key_vtable), UninitRow::new(value_vtable));
                unsafe {
                    map(
                        input.as_ptr(),
                        key_output.as_mut_ptr(),
                        value_output.as_mut_ptr(),
                    );
                    (key_output.assume_init(), value_output.assume_init())
                }
            })),

            MapFn::MapSet { map, key_vtable } => {
                RowStream::Set(input.unwrap_map().map(move |(key, value)| {
                    let mut key_output = UninitRow::new(key_vtable);
                    unsafe {
                        map(key.as_ptr(), value.as_ptr(), key_output.as_mut_ptr());
                        key_output.assume_init()
                    }
                }))
            }

            MapFn::MapMap {
                map,
                key_vtable,
                value_vtable,
            } => RowStream::Map(input.unwrap_map().map_index(move |(key, value)| {
                let (mut key_output, mut value_output) =
                    (UninitRow::new(key_vtable), UninitRow::new(value_vtable));
                unsafe {
                    map(
                        key.as_ptr(),
                        value.as_ptr(),
                        key_output.as_mut_ptr(),
                        value_output.as_mut_ptr(),
                    );
                    (key_output.assume_init(), value_output.assume_init())
                }
            })),
        };

        streams.insert(node_id, mapped);
    }
}

unsafe fn row_from_literal(
    literal: &RowLiteral,
    vtable: &'static VTable,
    layout: &NativeLayout,
) -> Row {
    let mut row = UninitRow::new(vtable);

    for (idx, column) in literal.rows().iter().enumerate() {
        match column {
            NullableConstant::NonNull(constant) => unsafe {
                let column_ptr = row.as_mut_ptr().add(layout.offset_of(idx) as usize);
                write_constant_to(constant, column_ptr);
            },

            NullableConstant::Nullable(constant) => {
                row.set_column_null(idx, layout, constant.is_none());

                if let Some(constant) = constant {
                    unsafe {
                        let column_ptr = row.as_mut_ptr().add(layout.offset_of(idx) as usize);

                        write_constant_to(constant, column_ptr)
                    }
                }
            }
        }
    }

    unsafe { row.assume_init() }
}

unsafe fn write_constant_to(constant: &Constant, ptr: *mut u8) {
    match *constant {
        Constant::Unit => ptr.cast::<()>().write(()),

        Constant::U8(value) => ptr.cast::<u8>().write(value),
        Constant::I8(value) => ptr.cast::<i8>().write(value),

        Constant::U16(value) => ptr.cast::<u16>().write(value),
        Constant::I16(value) => ptr.cast::<i16>().write(value),

        Constant::U32(value) => ptr.cast::<u32>().write(value),
        Constant::I32(value) => ptr.cast::<i32>().write(value),

        Constant::U64(value) => ptr.cast::<u64>().write(value),
        Constant::I64(value) => ptr.cast::<i64>().write(value),

        Constant::Usize(value) => ptr.cast::<usize>().write(value),
        Constant::Isize(value) => ptr.cast::<isize>().write(value),

        Constant::F32(value) => ptr.cast::<f32>().write(value),
        Constant::F64(value) => ptr.cast::<f64>().write(value),

        Constant::Bool(value) => ptr.cast::<bool>().write(value),

        Constant::String(ref value) => ptr.cast::<ThinStr>().write(ThinStr::from(&**value)),
        // Constant::Date(date) => ptr.cast::<i32>().write(date),
        // Constant::Timestamp(timestamp) => ptr.cast::<i64>().write(timestamp),
    }
}
