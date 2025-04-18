use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::time::Instant;

use dataflow_state::PointKey;
use derive_more::From;
use metrics::{counter, histogram};
use readyset_client::metrics::recorded;
use readyset_client::KeyComparison;
use readyset_errors::ReadySetResult;
use serde::{Deserialize, Serialize};

use crate::prelude::*;

pub mod filter;
pub mod grouped;
pub mod identity;
pub mod join;
pub mod paginate;
pub mod project;
pub mod topk;
pub mod union;
pub(crate) mod utils;

use crate::ops::grouped::concat::GroupConcat;
use crate::processing::{
    ColumnMiss, ColumnSource, IngredientLookupResult, LookupIndex, LookupMode,
};

/// Enum for distinguishing between the two parents of a union or join
#[derive(Debug, Eq, PartialEq, Clone, Copy, Hash, Serialize, Deserialize)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    fn other_side(&self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, From)]
#[allow(clippy::large_enum_variant)]
pub enum NodeOperator {
    // Aggregation was previously named "Sum", but now supports Sum, Count, Avg
    // Aggregation supports both filtered and normal Aggregations
    Aggregation(grouped::GroupedOperator<grouped::aggregate::Aggregator>),
    Extremum(grouped::GroupedOperator<grouped::extremum::ExtremumOperator>),
    Concat(grouped::GroupedOperator<GroupConcat>),
    Join(join::Join),
    Paginate(paginate::Paginate),
    Project(project::Project),
    Union(union::Union),
    Identity(identity::Identity),
    Filter(filter::Filter),
    TopK(topk::TopK),
}

impl Display for NodeOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            NodeOperator::Aggregation(_) => write!(f, "Aggregation"),
            NodeOperator::Extremum(_) => write!(f, "Extremum"),
            NodeOperator::Concat(_) => write!(f, "Concat"),
            NodeOperator::Join(_) => write!(f, "Join"),
            NodeOperator::Paginate(_) => write!(f, "Paginate"),
            NodeOperator::Project(_) => write!(f, "Project"),
            NodeOperator::Union(_) => write!(f, "Union"),
            NodeOperator::Identity(_) => write!(f, "Identity"),
            NodeOperator::Filter(_) => write!(f, "Filter"),
            NodeOperator::TopK(_) => write!(f, "TopK"),
        }
    }
}

macro_rules! impl_ingredient_fn_mut {
    ($self:ident, $fn:ident, $( $arg:ident ),* ) => {
        match $self {
            NodeOperator::Aggregation(i) => i.$fn($($arg),*),
            NodeOperator::Extremum(i) => i.$fn($($arg),*),
            NodeOperator::Concat(i) => i.$fn($($arg),*),
            NodeOperator::Join(i) => i.$fn($($arg),*),
            NodeOperator::Paginate(i) => i.$fn($($arg),*),
            NodeOperator::Project(i) => i.$fn($($arg),*),
            NodeOperator::Union(i) => i.$fn($($arg),*),
            NodeOperator::Identity(i) => i.$fn($($arg),*),
            NodeOperator::Filter(i) => i.$fn($($arg),*),
            NodeOperator::TopK(i) => i.$fn($($arg),*),
        }
    }
}

macro_rules! impl_ingredient_fn_ref {
    ($self:ident, $fn:ident, $( $arg:ident ),* ) => {
        match $self {
            NodeOperator::Aggregation(i) => i.$fn($($arg),*),
            NodeOperator::Extremum(i) => i.$fn($($arg),*),
            NodeOperator::Concat(i) => i.$fn($($arg),*),
            NodeOperator::Join(i) => i.$fn($($arg),*),
            NodeOperator::Paginate(i) => i.$fn($($arg),*),
            NodeOperator::Project(i) => i.$fn($($arg),*),
            NodeOperator::Union(i) => i.$fn($($arg),*),
            NodeOperator::Identity(i) => i.$fn($($arg),*),
            NodeOperator::Filter(i) => i.$fn($($arg),*),
            NodeOperator::TopK(i) => i.$fn($($arg),*),
        }
    }
}

impl Ingredient for NodeOperator {
    fn ancestors(&self) -> Vec<NodeIndex> {
        impl_ingredient_fn_ref!(self, ancestors,)
    }

    fn must_replay_among(&self) -> Option<HashSet<NodeIndex>> {
        impl_ingredient_fn_ref!(self, must_replay_among,)
    }

    fn suggest_indexes(&self, you: NodeIndex) -> HashMap<NodeIndex, LookupIndex> {
        impl_ingredient_fn_ref!(self, suggest_indexes, you)
    }

    fn column_source(&self, cols: &[usize]) -> ColumnSource {
        impl_ingredient_fn_ref!(self, column_source, cols)
    }

    fn handle_upquery(&mut self, miss: ColumnMiss) -> ReadySetResult<Vec<ColumnMiss>> {
        impl_ingredient_fn_mut!(self, handle_upquery, miss)
    }

    fn is_join(&self) -> bool {
        impl_ingredient_fn_ref!(self, is_join,)
    }

    fn description(&self) -> String {
        impl_ingredient_fn_ref!(self, description,)
    }

    fn probe(&self) -> HashMap<String, String> {
        impl_ingredient_fn_ref!(self, probe,)
    }

    fn on_connected(&mut self, graph: &Graph) {
        impl_ingredient_fn_mut!(self, on_connected, graph)
    }

    fn replace_sibling(&mut self, from_idx: NodeIndex, to_idx: NodeIndex) {
        impl_ingredient_fn_mut!(self, replace_sibling, from_idx, to_idx)
    }

    fn on_commit(&mut self, you: NodeIndex, remap: &HashMap<NodeIndex, IndexPair>) {
        impl_ingredient_fn_mut!(self, on_commit, you, remap)
    }

    fn on_input(
        &mut self,
        from: LocalNodeIndex,
        data: Records,
        replay: &ReplayContext,
        domain: &DomainNodes,
        states: &StateMap,
        auxiliary_node_states: &mut AuxiliaryNodeStateMap,
    ) -> ReadySetResult<ProcessingResult> {
        let start = Instant::now();
        let result = impl_ingredient_fn_mut!(
            self,
            on_input,
            from,
            data,
            replay,
            domain,
            states,
            auxiliary_node_states
        );

        let elapsed = start.elapsed().as_micros();
        histogram!(recorded::NODE_ON_INPUT_DURATION,
            "ntype" => self.to_string()
        )
        .record(elapsed as f64);
        counter!(recorded::NODE_ON_INPUT_INVOCATIONS, "ntype" => self.to_string()).increment(1);

        result
    }

    fn on_input_raw(
        &mut self,
        from: LocalNodeIndex,
        data: Records,
        replay: ReplayContext,
        domain: &DomainNodes,
        states: &StateMap,
        auxiliary_node_states: &mut AuxiliaryNodeStateMap,
    ) -> ReadySetResult<RawProcessingResult> {
        impl_ingredient_fn_mut!(
            self,
            on_input_raw,
            from,
            data,
            replay,
            domain,
            states,
            auxiliary_node_states
        )
    }

    fn on_eviction(&mut self, from: LocalNodeIndex, tag: Tag, keys: &[KeyComparison]) {
        impl_ingredient_fn_mut!(self, on_eviction, from, tag, keys)
    }

    fn can_query_through(&self) -> bool {
        impl_ingredient_fn_ref!(self, can_query_through,)
    }

    #[allow(clippy::type_complexity)]
    fn query_through<'a>(
        &self,
        columns: &[usize],
        key: &PointKey,
        nodes: &DomainNodes,
        states: &'a StateMap,
        mode: LookupMode,
    ) -> ReadySetResult<IngredientLookupResult<'a>> {
        impl_ingredient_fn_ref!(self, query_through, columns, key, nodes, states, mode)
    }

    #[allow(clippy::type_complexity)]
    fn lookup<'a>(
        &self,
        parent: LocalNodeIndex,
        columns: &[usize],
        key: &PointKey,
        domain: &DomainNodes,
        states: &'a StateMap,
        mode: LookupMode,
    ) -> ReadySetResult<IngredientLookupResult<'a>> {
        impl_ingredient_fn_ref!(self, lookup, parent, columns, key, domain, states, mode)
    }

    fn requires_full_materialization(&self) -> bool {
        impl_ingredient_fn_ref!(self, requires_full_materialization,)
    }
}

#[cfg(test)]
pub mod test {
    use std::cell;
    use std::collections::HashMap;

    use dataflow_state::MaterializedNodeState;
    use petgraph::graph::NodeIndex;

    use crate::node;
    use crate::prelude::*;
    use crate::processing::LookupIndex;
    use crate::utils::make_columns;

    pub(super) struct MockGraph {
        graph: Graph,
        source: NodeIndex,
        nut: Option<IndexPair>, // node under test
        pub(super) states: StateMap,
        nodes: DomainNodes,
        remap: HashMap<NodeIndex, IndexPair>,
        auxiliary_node_states: AuxiliaryNodeStateMap,
    }

    #[allow(clippy::new_without_default)]
    impl MockGraph {
        pub fn new() -> MockGraph {
            let mut graph = Graph::new();
            let source = graph.add_node(Node::new(
                "source",
                make_columns(&[""]),
                node::NodeType::Source,
            ));
            MockGraph {
                graph,
                source,
                nut: None,
                states: StateMap::new(),
                nodes: DomainNodes::default(),
                remap: HashMap::new(),
                auxiliary_node_states: Default::default(),
            }
        }

        pub fn add_base(&mut self, name: &str, fields: &[&str]) -> IndexPair {
            self.add_base_defaults(name, fields, vec![])
        }

        pub fn add_base_defaults(
            &mut self,
            name: &str,
            fields: &[&str],
            defaults: Vec<DfValue>,
        ) -> IndexPair {
            use crate::node::special::Base;
            let i = Base::new().with_default_values(defaults);
            let node = Node::new(name, make_columns(fields), i);
            if let Some(s) = node.initial_auxiliary_state() {
                self.auxiliary_node_states.insert(node.local_addr(), s);
            }
            let global = self.graph.add_node(node);
            self.graph.add_edge(self.source, global, ());
            let mut remap = HashMap::new();
            let local = LocalNodeIndex::make(self.remap.len() as u32);
            let mut ip: IndexPair = global.into();
            ip.set_local(local);
            self.graph
                .node_weight_mut(global)
                .unwrap()
                .set_finalized_addr(ip);
            remap.insert(global, ip);
            self.graph
                .node_weight_mut(global)
                .unwrap()
                .on_commit(&remap);
            self.states
                .insert(local, MaterializedNodeState::Memory(MemoryState::default()));
            self.remap.insert(global, ip);
            ip
        }

        pub fn set_op<I>(&mut self, name: &str, fields: &[&str], mut i: I, materialized: bool)
        where
            I: Ingredient + Into<NodeOperator>,
        {
            assert!(self.nut.is_none(), "only one node under test is supported");

            i.on_connected(&self.graph);
            let parents = i.ancestors();
            assert!(!parents.is_empty(), "node under test should have ancestors");

            let i: NodeOperator = i.into();
            let node = Node::new(name, make_columns(fields), i);
            let aux_state = node.initial_auxiliary_state();
            let global = self.graph.add_node(node);
            let local = LocalNodeIndex::make(self.remap.len() as u32);
            if let Some(aux_state) = aux_state {
                self.auxiliary_node_states.insert(local, aux_state);
            }
            if materialized {
                self.states
                    .insert(local, MaterializedNodeState::Memory(MemoryState::default()));
            }
            for parent in parents {
                self.graph.add_edge(parent, global, ());
            }
            let mut ip: IndexPair = global.into();
            ip.set_local(local);
            self.remap.insert(global, ip);
            self.graph
                .node_weight_mut(global)
                .unwrap()
                .set_finalized_addr(ip);
            self.graph
                .node_weight_mut(global)
                .unwrap()
                .on_commit(&self.remap);

            // we need to set the indices for all the base tables so they *actually* store things.
            let idx = self.graph[global].suggest_indexes(global);
            for (tbl, lookup_index) in idx {
                if let Some(ref mut s) = self.states.get_mut(self.graph[tbl].local_addr()) {
                    if lookup_index.is_weak() {
                        s.add_weak_index(lookup_index.index().clone())
                    }
                    s.add_index(lookup_index.into_index(), None);
                }
            }
            // and get rid of states we don't need
            let unused: Vec<_> = self
                .remap
                .values()
                .filter_map(|ni| {
                    let ni = self.graph[ni.as_global()].local_addr();
                    self.states.get(ni).map(move |s| (ni, !s.is_useful()))
                })
                .filter(|&(_, x)| x)
                .collect();
            for (ni, _) in unused {
                self.states.remove(ni);
            }

            // we're now committing to testing this op
            // add all nodes to the same domain
            for node in self.graph.node_weights_mut() {
                if node.is_source() {
                    continue;
                }
                node.add_to(0.into());
            }
            // store the id
            self.nut = Some(ip);
            // and also set up the node list
            let mut nodes = vec![];
            let mut topo = petgraph::visit::Topo::new(&self.graph);
            while let Some(node) = topo.next(&self.graph) {
                if node == self.source {
                    continue;
                }
                let n = self.graph[node].take();
                let n = n.finalize(&self.graph);
                nodes.push((node, n));
            }

            self.nodes = nodes
                .into_iter()
                .map(|(_, n)| (n.local_addr(), cell::RefCell::new(n)))
                .collect();
        }

        pub fn seed(&mut self, base: IndexPair, data: Vec<DfValue>) {
            assert!(self.nut.is_some(), "seed must happen after set_op");

            // base here is some identifier that was returned by Self::add_base.
            // which means it's a global address (and has to be so that it will correctly refer to
            // ancestors pre on_commit). we need to translate it into a local address.
            // since we set up the graph, we actually know that the NodeIndex is simply one greater
            // than the local index (since bases are added first, and assigned local + global
            // indices in order, but global ids are prefixed by the id of the source node).

            // no need to call on_input since base tables just forward anyway

            // if the base node has state, keep it
            if let Some(ref mut state) = self.states.get_mut(*base) {
                state
                    .process_records(&mut vec![data].into(), None, None)
                    .unwrap();
            } else {
                panic!(
                    "unnecessary seed value for {} (never used by any node)",
                    base.as_global().index()
                );
            }
        }

        #[allow(dead_code)]
        pub fn unseed(&mut self, base: IndexPair) {
            assert!(self.nut.is_some(), "unseed must happen after set_op");
            let global = self.nut.unwrap().as_global();
            let idx = self.graph[global].suggest_indexes(global);
            let mut state = MemoryState::default();
            for (tbl, lookup_index) in idx {
                if tbl == base.as_global() {
                    match lookup_index {
                        LookupIndex::Strict(index) => state.add_index(index, None),
                        LookupIndex::Weak(index) => state.add_weak_index(index),
                    }
                }
            }

            self.states
                .insert(*base, MaterializedNodeState::Memory(state));
        }

        pub fn input_raw<U: Into<Records>>(
            &mut self,
            src: IndexPair,
            u: U,
            replay: ReplayContext,
            remember: bool,
        ) -> RawProcessingResult {
            assert!(self.nut.is_some());
            assert!(!remember || self.states.contains_key(*self.nut.unwrap()));

            let tag = replay.tag();
            let mut res = {
                let id = self.nut.unwrap();
                let mut n = self.nodes[*id].borrow_mut();
                n.as_mut_internal()
                    .unwrap()
                    .on_input_raw(
                        *src,
                        u.into(),
                        replay,
                        &self.nodes,
                        &self.states,
                        &mut self.auxiliary_node_states,
                    )
                    .unwrap()
            };

            if !remember || !self.states.contains_key(*self.nut.unwrap()) {
                return res;
            }

            if let RawProcessingResult::Regular(ref mut res) = &mut res {
                node::materialize(
                    &mut res.results,
                    None,
                    tag,
                    self.states.get_mut(*self.nut.unwrap()),
                )
                .unwrap();
            }

            res
        }

        pub fn input<U: Into<Records>>(
            &mut self,
            src: IndexPair,
            u: U,
            remember: bool,
        ) -> ProcessingResult {
            match self.input_raw(src, u, ReplayContext::None, remember) {
                RawProcessingResult::Regular(res) => res,
                _ => panic!(),
            }
        }

        pub fn one<U: Into<Records>>(&mut self, src: IndexPair, u: U, remember: bool) -> Records {
            self.input(src, u, remember).results
        }

        pub fn one_row<R: Into<Record>>(
            &mut self,
            src: IndexPair,
            d: R,
            remember: bool,
        ) -> Records {
            self.one::<Record>(src, d.into(), remember)
        }

        pub fn narrow_one<U: Into<Records>>(&mut self, u: U, remember: bool) -> Records {
            let src = self.narrow_base_id();
            self.one(src, u.into(), remember)
        }

        pub fn narrow_one_row<R: Into<Record>>(&mut self, d: R, remember: bool) -> Records {
            self.narrow_one(d.into(), remember)
        }

        pub fn node_index(&self) -> IndexPair {
            self.nut.expect("set_op not called")
        }

        pub fn node(&self) -> cell::Ref<Node> {
            self.nodes[*self.nut.unwrap()].borrow()
        }

        pub fn node_mut(&self) -> cell::RefMut<Node> {
            self.nodes[*self.nut.unwrap()].borrow_mut()
        }

        pub fn narrow_base_id(&self) -> IndexPair {
            assert_eq!(self.remap.len(), 2 /* base + nut */);
            *self
                .remap
                .values()
                .find(|&n| n.as_global() != self.nut.unwrap().as_global())
                .unwrap()
        }
    }
}
