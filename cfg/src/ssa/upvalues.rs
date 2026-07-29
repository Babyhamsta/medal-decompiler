use std::collections::{TryReserveError, VecDeque};

use petgraph::stable_graph::NodeIndex;
use rangemap::RangeInclusiveMap;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::function::Function;

type OpenSite = (NodeIndex, usize);
type OpenState = FxHashMap<ast::RcLocal, EpochId>;
type SiteEpochs = FxHashMap<(ast::RcLocal, NodeIndex, usize), EpochId>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct EpochId(usize);

#[derive(Debug, thiserror::Error)]
pub enum UpvalueAnalysisError {
    #[error("unable to reserve bounded upvalue state: {0}")]
    Resource(String),
}

#[derive(Debug, Default)]
struct EpochForest {
    parents: Vec<EpochId>,
    sites: Vec<OpenSite>,
}

impl EpochForest {
    fn with_capacity(capacity: usize) -> Result<Self, UpvalueAnalysisError> {
        let mut parents = Vec::new();
        reserve(parents.try_reserve(capacity))?;
        let mut sites = Vec::new();
        reserve(sites.try_reserve(capacity))?;
        Ok(Self { parents, sites })
    }

    fn create(&mut self, site: OpenSite) -> EpochId {
        let id = EpochId(self.parents.len());
        self.parents.push(id);
        self.sites.push(site);
        id
    }

    fn find(&mut self, id: EpochId) -> EpochId {
        let mut root = id;
        while self.parents[root.0] != root {
            root = self.parents[root.0];
        }

        let mut current = id;
        while current != root {
            let parent = self.parents[current.0];
            self.parents[current.0] = root;
            current = parent;
        }
        root
    }

    fn union(&mut self, left: EpochId, right: EpochId) -> EpochId {
        let left = self.find(left);
        let right = self.find(right);
        if left == right {
            return left;
        }

        let left_site = self.sites[left.0];
        let right_site = self.sites[right.0];
        let left_key = (left_site.0.index(), left_site.1, left.0);
        let right_key = (right_site.0.index(), right_site.1, right.0);
        let (canonical, other) = if left_key <= right_key {
            (left, right)
        } else {
            (right, left)
        };
        self.parents[other.0] = canonical;
        canonical
    }

    fn site(&mut self, id: EpochId) -> OpenSite {
        let root = self.find(id);
        self.sites[root.0]
    }
}

#[derive(Debug)]
pub(crate) struct UpvaluesOpen {
    pub open: FxHashMap<
        NodeIndex,
        FxHashMap<ast::RcLocal, RangeInclusiveMap<usize, Vec<(NodeIndex, usize)>>>,
    >,
}

impl UpvaluesOpen {
    pub fn new(function: &Function, old_locals: FxHashMap<ast::RcLocal, ast::RcLocal>) -> Self {
        Self::try_new(function, old_locals).expect("bounded upvalue analysis")
    }

    pub fn try_new(
        function: &Function,
        old_locals: FxHashMap<ast::RcLocal, ast::RcLocal>,
    ) -> Result<Self, UpvalueAnalysisError> {
        let mut nodes = function.blocks().map(|(node, _)| node).collect::<Vec<_>>();
        nodes.sort_unstable_by_key(|node| node.index());

        let capture_count = nodes.iter().try_fold(0usize, |count, &node| {
            let references = function
                .block(node)
                .unwrap()
                .iter()
                .map(reference_capture_count)
                .sum::<usize>();
            count
                .checked_add(references)
                .ok_or_else(|| UpvalueAnalysisError::Resource("capture count overflow".into()))
        })?;

        let mut epochs = EpochForest::with_capacity(capture_count)?;
        let mut site_epochs = SiteEpochs::default();
        reserve(site_epochs.try_reserve(capture_count))?;
        let mut captured_locals = FxHashSet::default();
        reserve(captured_locals.try_reserve(capture_count))?;
        for &node in &nodes {
            for (statement, value) in function.block(node).unwrap().iter().enumerate() {
                for_each_reference_capture(value, &old_locals, |local| {
                    captured_locals.insert(local.clone());
                    let key = (local, node, statement);
                    if !site_epochs.contains_key(&key) {
                        let epoch = epochs.create((node, statement));
                        site_epochs.insert(key, epoch);
                    }
                });
            }
        }

        let captured_local_count = captured_locals.len();
        let mut entry_states = FxHashMap::default();
        reserve(entry_states.try_reserve(nodes.len()))?;
        let mut exit_states = FxHashMap::default();
        reserve(exit_states.try_reserve(nodes.len()))?;
        let mut worklist = VecDeque::new();
        reserve(worklist.try_reserve(nodes.len()))?;
        let mut queued = FxHashSet::default();
        reserve(queued.try_reserve(nodes.len()))?;
        for &node in &nodes {
            worklist.push_back(node);
            queued.insert(node);
        }

        while let Some(node) = worklist.pop_front() {
            queued.remove(&node);
            let incoming = merge_predecessors(
                function,
                node,
                &exit_states,
                &mut epochs,
                captured_local_count,
            )?;
            let mut outgoing = clone_state(&incoming)?;
            transfer_block(
                function,
                node,
                &old_locals,
                &site_epochs,
                &mut epochs,
                &mut outgoing,
            )?;

            let input_changed = entry_states
                .get(&node)
                .is_none_or(|previous| !states_equal(previous, &incoming, &mut epochs));
            let output_changed = exit_states
                .get(&node)
                .is_none_or(|previous| !states_equal(previous, &outgoing, &mut epochs));
            if input_changed {
                entry_states.insert(node, incoming);
            }
            if output_changed {
                exit_states.insert(node, outgoing);
                let mut successors = function.successor_blocks(node).collect::<Vec<_>>();
                successors.sort_unstable_by_key(|successor| successor.index());
                for successor in successors {
                    if queued.insert(successor) {
                        worklist.push_back(successor);
                    }
                }
            }
        }

        let open = materialize_ranges(
            function,
            &nodes,
            &entry_states,
            &old_locals,
            &site_epochs,
            &mut epochs,
        )?;
        Ok(Self { open })
    }

    pub fn opening_location(
        &self,
        node: NodeIndex,
        local: &ast::RcLocal,
        statement: usize,
    ) -> Option<OpenSite> {
        self.open
            .get(&node)?
            .get(local)?
            .get(&statement)?
            .first()
            .copied()
    }
}

fn reserve(result: Result<(), TryReserveError>) -> Result<(), UpvalueAnalysisError> {
    result.map_err(|error| UpvalueAnalysisError::Resource(error.to_string()))
}

fn reference_capture_count(statement: &ast::Statement) -> usize {
    statement
        .as_assign()
        .into_iter()
        .flat_map(|assign| &assign.right)
        .filter_map(|value| value.as_closure())
        .flat_map(|closure| &closure.upvalues)
        .filter(|upvalue| matches!(upvalue, ast::Upvalue::Ref(_)))
        .count()
}

fn for_each_reference_capture(
    statement: &ast::Statement,
    old_locals: &FxHashMap<ast::RcLocal, ast::RcLocal>,
    mut operation: impl FnMut(ast::RcLocal),
) {
    if let Some(assign) = statement.as_assign() {
        for local in assign
            .right
            .iter()
            .filter_map(|value| value.as_closure())
            .flat_map(|closure| &closure.upvalues)
            .filter_map(|upvalue| match upvalue {
                ast::Upvalue::Copy(_) => None,
                ast::Upvalue::Ref(local) => Some(local),
            })
        {
            operation(old_locals[local].clone());
        }
    }
}

fn empty_state(capacity: usize) -> Result<OpenState, UpvalueAnalysisError> {
    let mut state = OpenState::default();
    reserve(state.try_reserve(capacity))?;
    Ok(state)
}

fn clone_state(source: &OpenState) -> Result<OpenState, UpvalueAnalysisError> {
    let mut state = empty_state(source.len())?;
    state.extend(source.iter().map(|(local, epoch)| (local.clone(), *epoch)));
    Ok(state)
}

fn states_equal(left: &OpenState, right: &OpenState, epochs: &mut EpochForest) -> bool {
    left.len() == right.len()
        && left.iter().all(|(local, left_epoch)| {
            right
                .get(local)
                .is_some_and(|right_epoch| epochs.find(*left_epoch) == epochs.find(*right_epoch))
        })
}

fn merge_predecessors(
    function: &Function,
    node: NodeIndex,
    exit_states: &FxHashMap<NodeIndex, OpenState>,
    epochs: &mut EpochForest,
    captured_local_count: usize,
) -> Result<OpenState, UpvalueAnalysisError> {
    let mut predecessors = function.predecessor_blocks(node).collect::<Vec<_>>();
    predecessors.sort_unstable_by_key(|predecessor| predecessor.index());
    let reserve_count = predecessors
        .iter()
        .filter_map(|predecessor| exit_states.get(predecessor))
        .try_fold(0usize, |count, state| count.checked_add(state.len()))
        .ok_or_else(|| UpvalueAnalysisError::Resource("merge state count overflow".into()))?
        .min(captured_local_count);
    let mut incoming = empty_state(reserve_count)?;
    for predecessor in predecessors {
        if let Some(state) = exit_states.get(&predecessor) {
            for (local, epoch) in state {
                let epoch = epochs.find(*epoch);
                if let Some(previous) = incoming.get(local).copied() {
                    incoming.insert(local.clone(), epochs.union(previous, epoch));
                } else {
                    incoming.insert(local.clone(), epoch);
                }
            }
        }
    }
    Ok(incoming)
}

fn transfer_block(
    function: &Function,
    node: NodeIndex,
    old_locals: &FxHashMap<ast::RcLocal, ast::RcLocal>,
    site_epochs: &SiteEpochs,
    epochs: &mut EpochForest,
    state: &mut OpenState,
) -> Result<(), UpvalueAnalysisError> {
    let block = function.block(node).unwrap();
    let additional = block.iter().map(reference_capture_count).sum();
    reserve(state.try_reserve(additional))?;
    for (statement, value) in block.iter().enumerate() {
        for_each_reference_capture(value, old_locals, |local| {
            let site_epoch = site_epochs[&(local.clone(), node, statement)];
            let epoch = state
                .get(&local)
                .copied()
                .map_or(site_epoch, |active| epochs.union(active, site_epoch));
            state.insert(local, epoch);
        });
        if let ast::Statement::Close(close) = value {
            for local in &close.locals {
                state.remove(local);
            }
        }
    }
    Ok(())
}

fn materialize_ranges(
    function: &Function,
    nodes: &[NodeIndex],
    entry_states: &FxHashMap<NodeIndex, OpenState>,
    old_locals: &FxHashMap<ast::RcLocal, ast::RcLocal>,
    site_epochs: &SiteEpochs,
    epochs: &mut EpochForest,
) -> Result<
    FxHashMap<
        NodeIndex,
        FxHashMap<ast::RcLocal, RangeInclusiveMap<usize, Vec<(NodeIndex, usize)>>>,
    >,
    UpvalueAnalysisError,
> {
    let mut open = FxHashMap::default();
    reserve(open.try_reserve(nodes.len()))?;
    for &node in nodes {
        let block = function.block(node).unwrap();
        let end = block.len().saturating_sub(1);
        let mut state = if let Some(entry) = entry_states.get(&node) {
            clone_state(entry)?
        } else {
            empty_state(0)?
        };
        let capture_count = block.iter().map(reference_capture_count).sum::<usize>();
        reserve(state.try_reserve(capture_count))?;
        let range_capacity = state
            .len()
            .checked_add(capture_count)
            .ok_or_else(|| UpvalueAnalysisError::Resource("range state count overflow".into()))?;
        let mut block_opened = FxHashMap::default();
        reserve(block_opened.try_reserve(range_capacity))?;
        for (local, epoch) in &state {
            let mut ranges = RangeInclusiveMap::new();
            ranges.insert(0..=end, vec![epochs.site(*epoch)]);
            block_opened.insert(local.clone(), ranges);
        }

        for (statement, value) in block.iter().enumerate() {
            for_each_reference_capture(value, old_locals, |local| {
                let site_epoch = site_epochs[&(local.clone(), node, statement)];
                if let Some(active) = state.get(&local).copied() {
                    state.insert(local, epochs.union(active, site_epoch));
                } else {
                    state.insert(local.clone(), site_epoch);
                    block_opened
                        .entry(local)
                        .or_insert_with(RangeInclusiveMap::new)
                        .insert(statement..=end, vec![epochs.site(site_epoch)]);
                }
            });
            if let ast::Statement::Close(close) = value {
                for local in &close.locals {
                    state.remove(local);
                    if let Some(ranges) = block_opened.get_mut(local) {
                        ranges.remove(statement..=end);
                    }
                }
            }
        }
        open.insert(node, block_opened);
    }
    Ok(open)
}

#[cfg(test)]
mod tests {
    use ast::{Assign, Close, Closure, Literal, RcLocal, Upvalue};
    use rustc_hash::FxHashMap;

    use crate::{
        block::{BlockEdge, BranchType},
        function::Function,
    };

    use super::UpvaluesOpen;

    fn capture(local: &RcLocal) -> ast::Statement {
        Assign::new(
            vec![RcLocal::default().into()],
            vec![
                Closure {
                    function: Default::default(),
                    upvalues: vec![Upvalue::Ref(local.clone())],
                }
                .into(),
            ],
        )
        .into()
    }

    fn marker() -> ast::Statement {
        Assign::new(vec![RcLocal::default().into()], vec![Literal::Nil.into()]).into()
    }

    fn identity_map(local: &RcLocal) -> FxHashMap<RcLocal, RcLocal> {
        FxHashMap::from_iter([(local.clone(), local.clone())])
    }

    fn diamond_chain(depth: usize) -> (Function, RcLocal) {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(capture(&captured));
        let mut tail = entry;

        for _ in 0..depth {
            let detour = function.new_block();
            let merge = function.new_block();
            function
                .graph_mut()
                .add_edge(tail, detour, BlockEdge::new(BranchType::Else));
            function
                .graph_mut()
                .add_edge(tail, merge, BlockEdge::new(BranchType::Then));
            function
                .graph_mut()
                .add_edge(detour, merge, BlockEdge::default());
            tail = merge;
        }

        (function, captured)
    }

    #[test]
    fn diamond_paths_keep_one_canonical_opening() {
        let (function, captured) = diamond_chain(8);
        let old_locals = FxHashMap::from_iter([(captured.clone(), captured)]);

        let analysis = UpvaluesOpen::new(&function, old_locals);
        let maximum_locations = analysis
            .open
            .values()
            .flat_map(|locals| locals.values())
            .flat_map(|ranges| ranges.iter().map(|(_, locations)| locations.len()))
            .max()
            .unwrap_or_default();

        assert_eq!(maximum_locations, 1);
    }

    #[test]
    fn merge_uses_the_first_capture_as_the_canonical_opening() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let left = function.new_block();
        let right = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function.block_mut(left).unwrap().push(capture(&captured));
        function.block_mut(right).unwrap().push(capture(&captured));
        function.block_mut(merge).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, left, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, right, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(left, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(right, merge, BlockEdge::default());

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(
            analysis.opening_location(merge, &captured, 0),
            Some((left, 0))
        );
    }

    #[test]
    fn loop_carries_the_open_capture_over_its_back_edge() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let header = function.new_block();
        let body = function.new_block();
        let after_loop = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(capture(&captured));
        function.block_mut(header).unwrap().push(marker());
        function.block_mut(body).unwrap().push(marker());
        function.block_mut(after_loop).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, header, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(header, body, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(header, after_loop, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(body, header, BlockEdge::default());

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(
            analysis.opening_location(after_loop, &captured, 0),
            Some((entry, 0))
        );
    }

    #[test]
    fn close_ends_the_capture_epoch_before_successor_statements() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let close = function.new_block();
        let after_close = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(capture(&captured));
        function.block_mut(close).unwrap().push(
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
        );
        function.block_mut(after_close).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, close, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(close, after_close, BlockEdge::default());

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(analysis.opening_location(after_close, &captured, 0), None);
    }
}
