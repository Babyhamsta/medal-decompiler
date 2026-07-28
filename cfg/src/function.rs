use ast::{LocalRw, RcLocal};
use contracts::requires;
use rustc_hash::FxHashMap;

use petgraph::{
    Direction,
    stable_graph::{EdgeReference, Neighbors, NodeIndex, StableDiGraph},
    visit::{EdgeRef, IntoEdgesDirected},
};

use crate::{
    block::{BlockEdge, BranchType},
    provenance::{BindingIdentity, OriginSet, Provenance, RegisterFamily},
};

#[derive(Debug, Clone, Default)]
pub struct Function {
    pub id: usize,
    pub name: Option<String>,
    pub parameters: Vec<RcLocal>,
    pub is_variadic: bool,
    graph: StableDiGraph<ast::Block, BlockEdge>,
    entry: Option<NodeIndex>,
    provenance: Provenance,
    bindings: FxHashMap<RcLocal, BindingIdentity>,
    register_families: FxHashMap<RcLocal, RegisterFamily>,
    statement_origins: FxHashMap<NodeIndex, Vec<OriginSet>>,
    next_synthetic_binding: usize,
}

impl Function {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            name: None,
            parameters: Vec::new(),
            is_variadic: false,
            graph: StableDiGraph::new(),
            entry: None,
            provenance: Provenance::default(),
            bindings: FxHashMap::default(),
            register_families: FxHashMap::default(),
            statement_origins: FxHashMap::default(),
            next_synthetic_binding: 0,
        }
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub fn set_binding(&mut self, local: RcLocal, binding: BindingIdentity) {
        self.bindings.insert(local.clone(), binding.clone());
        self.provenance.ensure_local(local, binding);
    }

    pub fn set_register_family(&mut self, local: RcLocal, family: RegisterFamily) {
        self.bindings.insert(local.clone(), family.binding.clone());
        self.register_families.insert(local, family);
    }

    pub fn set_statement_origins(&mut self, node: NodeIndex, origins: Vec<OriginSet>) {
        assert_eq!(
            self.block(node).map_or(0, |block| block.len()),
            origins.len()
        );
        self.statement_origins.insert(node, origins);
    }

    pub fn statement_origins(&self, node: NodeIndex, statement: usize) -> Option<&OriginSet> {
        self.statement_origins
            .get(&node)
            .and_then(|origins| origins.get(statement))
    }

    pub fn new_ssa_definition(
        &mut self,
        family_local: &RcLocal,
        node: NodeIndex,
        statement: usize,
    ) -> RcLocal {
        let base_origins = self
            .statement_origins(node, statement)
            .cloned()
            .expect("lifted statement must retain its source origin");
        let family = self.register_families.get(family_local).cloned();
        let (definitions, name) = if let Some(family) = family {
            let names = base_origins
                .iter()
                .filter_map(|origin| family.debug_name_at(origin.instruction))
                .filter(|name| ast::is_valid_identifier(name))
                .filter_map(|name| String::from_utf8(name.to_vec()).ok())
                .collect::<std::collections::BTreeSet<_>>();
            let definitions = base_origins
                .into_iter()
                .map(|origin| {
                    (
                        family.binding_at(origin.instruction),
                        family.definition_origin(origin),
                    )
                })
                .collect::<Vec<_>>();
            let name = (names.len() == 1).then(|| names.into_iter().next().unwrap());
            (definitions, name)
        } else {
            let binding = self
                .bindings
                .get(family_local)
                .cloned()
                .expect("local must have a binding identity");
            (
                base_origins
                    .into_iter()
                    .map(|origin| (binding.clone(), origin))
                    .collect(),
                None,
            )
        };
        let local = RcLocal::new(ast::Local::new(name));
        self.bindings.insert(
            local.clone(),
            self.bindings
                .get(family_local)
                .cloned()
                .expect("local must have a binding identity"),
        );
        for (binding, origin) in definitions {
            self.provenance
                .record_definition(local.clone(), binding, origin);
        }
        local
    }

    pub fn new_merge_local(&mut self, family_local: &RcLocal) -> RcLocal {
        let local = RcLocal::default();
        let binding = self
            .bindings
            .get(family_local)
            .cloned()
            .expect("local must have a binding identity");
        self.bindings.insert(local.clone(), binding.clone());
        self.provenance.ensure_local(local.clone(), binding);
        local
    }

    pub fn new_synthetic_local(&mut self, source: &RcLocal) -> RcLocal {
        let name = source
            .0
            .0
            .lock()
            .0
            .clone()
            .filter(|name| ast::is_valid_identifier(name.as_bytes()));
        let local = RcLocal::new(ast::Local::new(name));
        let binding = BindingIdentity::SyntheticLocal {
            function_id: self.id,
            sequence: self.next_synthetic_binding,
        };
        self.next_synthetic_binding += 1;
        self.bindings.insert(local.clone(), binding.clone());
        self.provenance
            .derive_local(local.clone(), binding, [source]);
        local
    }

    pub fn record_use(&mut self, local: &RcLocal, node: NodeIndex, statement: usize) {
        if let Some(origins) = self.statement_origins(node, statement).cloned() {
            let binding = self
                .provenance
                .binding(local)
                .or_else(|| self.bindings.get(local))
                .cloned();
            for mut origin in origins {
                match binding.as_ref() {
                    Some(BindingIdentity::Local {
                        register, lifetime, ..
                    }) => {
                        origin.register_family = Some(*register);
                        origin.debug_lifetime =
                            lifetime.and_then(|(start_instruction, end_instruction)| {
                                self.register_families
                                    .values()
                                    .flat_map(|family| family.debug_lifetimes.iter())
                                    .find(|candidate| {
                                        candidate.start_instruction == start_instruction
                                            && candidate.end_instruction == end_instruction
                                            && candidate.contains(origin.instruction)
                                    })
                                    .cloned()
                            });
                    }
                    Some(BindingIdentity::Parameter { register, .. }) => {
                        origin.register_family = Some(*register);
                    }
                    _ => {}
                }
                self.provenance.record_use(local.clone(), origin);
            }
        }
    }

    pub fn merge_local_provenance<'a>(
        &mut self,
        target: RcLocal,
        sources: impl IntoIterator<Item = &'a RcLocal>,
    ) {
        let binding = self
            .bindings
            .get(&target)
            .cloned()
            .or_else(|| self.provenance.binding(&target).cloned())
            .expect("merged local must have a binding identity");
        self.provenance
            .merge_locals(target.clone(), binding.clone(), sources);
        self.bindings.insert(target, binding);
    }

    pub fn name_mut(&mut self) -> &mut Option<String> {
        &mut self.name
    }

    pub fn entry(&self) -> &Option<NodeIndex> {
        &self.entry
    }

    #[requires(self.has_block(new_entry))]
    pub fn set_entry(&mut self, new_entry: NodeIndex) {
        self.entry = Some(new_entry);
    }

    pub fn graph(&self) -> &StableDiGraph<ast::Block, BlockEdge> {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut StableDiGraph<ast::Block, BlockEdge> {
        &mut self.graph
    }

    pub fn has_block(&self, block: NodeIndex) -> bool {
        self.graph.contains_node(block)
    }

    pub fn block(&self, block: NodeIndex) -> Option<&ast::Block> {
        self.graph.node_weight(block)
    }

    pub fn block_mut(&mut self, block: NodeIndex) -> Option<&mut ast::Block> {
        self.graph.node_weight_mut(block)
    }

    pub fn blocks(&self) -> impl Iterator<Item = (NodeIndex, &ast::Block)> {
        self.graph
            .node_indices()
            .map(|i| (i, self.graph.node_weight(i).unwrap()))
    }

    pub fn blocks_mut(&mut self) -> impl Iterator<Item = &mut ast::Block> {
        self.graph.node_weights_mut()
    }

    pub fn successor_blocks(&self, block: NodeIndex) -> Neighbors<BlockEdge> {
        self.graph.neighbors_directed(block, Direction::Outgoing)
    }

    pub fn predecessor_blocks(&self, block: NodeIndex) -> Neighbors<BlockEdge> {
        self.graph.neighbors_directed(block, Direction::Incoming)
    }

    pub fn edges_to_block(&self, node: NodeIndex) -> impl Iterator<Item = (NodeIndex, &BlockEdge)> {
        let mut edges = self.predecessor_blocks(node).detach();
        std::iter::from_fn(move || edges.next_edge(&self.graph)).filter_map(move |e| {
            let (source, target) = self.graph.edge_endpoints(e).unwrap();
            if target == node {
                Some((source, self.graph.edge_weight(e).unwrap()))
            } else {
                None
            }
        })
    }

    pub fn edges(&self, node: NodeIndex) -> impl Iterator<Item = EdgeReference<BlockEdge>> {
        self.graph.edges_directed(node, Direction::Outgoing)
    }

    pub fn remove_edges(&mut self, node: NodeIndex) -> Vec<(NodeIndex, BlockEdge)> {
        let mut edges = Vec::new();
        for (target, edge) in self
            .edges(node)
            .map(|e| (e.target(), e.id()))
            .collect::<Vec<_>>()
        {
            edges.push((target, self.graph.remove_edge(edge).unwrap()));
        }
        edges
    }

    // returns previous edges
    pub fn set_edges(
        &mut self,
        node: NodeIndex,
        new_edges: Vec<(NodeIndex, BlockEdge)>,
    ) -> Vec<(NodeIndex, BlockEdge)> {
        let prev_edges = self.remove_edges(node);
        for (target, edge) in new_edges {
            self.graph.add_edge(node, target, edge);
        }
        prev_edges
    }

    pub fn conditional_edges(
        &self,
        node: NodeIndex,
    ) -> Option<(EdgeReference<BlockEdge>, EdgeReference<BlockEdge>)> {
        let edges = self
            .graph
            .edges_directed(node, Direction::Outgoing)
            .collect::<Vec<_>>();
        if let [e0, e1] = edges[..] {
            let mut res = (e0, e1);
            if res.1.weight().branch_type == BranchType::Then {
                std::mem::swap(&mut res.0, &mut res.1);
            }
            assert!(res.0.weight().branch_type == BranchType::Then);
            assert!(res.1.weight().branch_type == BranchType::Else);
            Some(res)
        } else {
            None
        }
    }

    pub fn unconditional_edge(&self, node: NodeIndex) -> Option<EdgeReference<BlockEdge>> {
        let edges = self
            .graph
            .edges_directed(node, Direction::Outgoing)
            .collect::<Vec<_>>();
        if let [e] = edges[..] { Some(e) } else { None }
    }

    // TODO: disable_contracts for production builds
    #[requires(self.has_block(node))]
    pub fn values_read(&self, node: NodeIndex) -> impl Iterator<Item = &RcLocal> {
        self.block(node)
            .unwrap()
            .0
            .iter()
            .flat_map(|s| s.values_read())
            .chain(self.edges(node).flat_map(|e| {
                e.weight()
                    .arguments
                    .iter()
                    .flat_map(|(_, a)| a.values_read())
            }))
    }

    pub fn new_block(&mut self) -> NodeIndex {
        self.graph.add_node(ast::Block::default())
    }

    pub fn remove_block(&mut self, block: NodeIndex) -> Option<ast::Block> {
        self.statement_origins.remove(&block);
        self.graph.remove_node(block)
    }
}
