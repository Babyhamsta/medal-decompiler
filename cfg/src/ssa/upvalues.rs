use std::{
    collections::{TryReserveError, VecDeque},
    ops::RangeInclusive,
};

use petgraph::stable_graph::NodeIndex;
use petgraph::visit::{Dfs, Walker};
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
    #[error("path-dependent upvalue openness at control-flow merge block {block}")]
    PathDependentMerge { block: usize },
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

#[derive(Debug, Default)]
struct OpenRanges(Vec<(RangeInclusive<usize>, OpenSite)>);

impl OpenRanges {
    fn with_capacity(capacity: usize) -> Result<Self, UpvalueAnalysisError> {
        let mut ranges = Vec::new();
        reserve(ranges.try_reserve(capacity))?;
        Ok(Self(ranges))
    }

    fn push(&mut self, range: RangeInclusive<usize>, site: OpenSite) {
        self.0.push((range, site));
    }

    fn close_from(&mut self, statement: usize) {
        let Some((range, _)) = self.0.last_mut() else {
            return;
        };
        if !range.contains(&statement) {
            return;
        }
        let start = *range.start();
        if start < statement {
            *range = start..=statement - 1;
        } else {
            self.0.pop();
        }
    }

    fn get(&self, statement: usize) -> Option<OpenSite> {
        self.0
            .iter()
            .find_map(|(range, site)| range.contains(&statement).then_some(*site))
    }
}

#[derive(Debug)]
pub(crate) struct UpvaluesOpen {
    open: FxHashMap<NodeIndex, FxHashMap<ast::RcLocal, OpenRanges>>,
}

impl UpvaluesOpen {
    pub fn try_new(
        function: &Function,
        old_locals: FxHashMap<ast::RcLocal, ast::RcLocal>,
    ) -> Result<Self, UpvalueAnalysisError> {
        let entry = (*function.entry()).ok_or_else(|| {
            UpvalueAnalysisError::Resource("function has no control-flow entry".into())
        })?;
        let mut nodes = Vec::new();
        reserve(nodes.try_reserve(function.graph().node_count()))?;
        nodes.extend(Dfs::new(function.graph(), entry).iter(function.graph()));
        nodes.sort_unstable_by_key(|node| node.index());

        let capture_count = nodes.iter().try_fold(0usize, |count, &node| {
            let references = block_capture_count(function.block(node).unwrap())?;
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
                let successor_count = function.successor_blocks(node).count();
                let mut successors = Vec::new();
                reserve(successors.try_reserve(successor_count))?;
                successors.extend(function.successor_blocks(node));
                successors.sort_unstable_by_key(|successor| successor.index());
                for successor in successors {
                    if queued.insert(successor) {
                        worklist.push_back(successor);
                    }
                }
            }
        }

        validate_merge_openness(
            function,
            &nodes,
            &exit_states,
            &old_locals,
            &site_epochs,
            &mut epochs,
            captured_local_count,
        )?;
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
        self.open.get(&node)?.get(local)?.get(statement)
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

fn block_capture_count(block: &ast::Block) -> Result<usize, UpvalueAnalysisError> {
    block.iter().try_fold(0usize, |count, statement| {
        count
            .checked_add(reference_capture_count(statement))
            .ok_or_else(|| UpvalueAnalysisError::Resource("block capture count overflow".into()))
    })
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
    let predecessor_count = function.predecessor_blocks(node).count();
    let mut predecessors = Vec::new();
    reserve(predecessors.try_reserve(predecessor_count))?;
    predecessors.extend(function.predecessor_blocks(node));
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

fn validate_merge_openness(
    function: &Function,
    nodes: &[NodeIndex],
    exit_states: &FxHashMap<NodeIndex, OpenState>,
    old_locals: &FxHashMap<ast::RcLocal, ast::RcLocal>,
    site_epochs: &SiteEpochs,
    epochs: &mut EpochForest,
    captured_local_count: usize,
) -> Result<(), UpvalueAnalysisError> {
    let mut presence = FxHashMap::default();
    reserve(presence.try_reserve(captured_local_count))?;
    let mut validated = FxHashSet::default();
    reserve(validated.try_reserve(captured_local_count))?;
    for &node in nodes {
        presence.clear();
        let mut predecessor_count = 0usize;
        for predecessor in function.predecessor_blocks(node) {
            let Some(state) = exit_states.get(&predecessor) else {
                continue;
            };
            predecessor_count = predecessor_count.checked_add(1).ok_or_else(|| {
                UpvalueAnalysisError::Resource("predecessor count overflow".into())
            })?;
            for (local, epoch) in state {
                let entry = presence
                    .entry(local.clone())
                    .or_insert((0usize, *epoch, false));
                entry.0 = entry.0.checked_add(1).ok_or_else(|| {
                    UpvalueAnalysisError::Resource("merge presence count overflow".into())
                })?;
                entry.2 |= entry.1 != *epoch;
            }
        }
        if predecessor_count <= 1 {
            continue;
        }
        for (local, &(open_count, _, distinct_epoch)) in &presence {
            if open_count == predecessor_count && !distinct_epoch {
                continue;
            }
            if validated.insert(local.clone()) {
                validate_local_merge_openness(
                    function,
                    nodes,
                    exit_states,
                    old_locals,
                    site_epochs,
                    epochs,
                    local,
                )?;
            }
        }
    }
    Ok(())
}

/// Rejects merges where a capture epoch is live on one path and already
/// detached on another.
///
/// Divergent openness is only ambiguous when the *same* epoch is involved: a
/// merge that joins an open epoch with a path on which that epoch was closed
/// cannot decide whether writes after the merge are still observed by the
/// closure created at that epoch's capture site. Joining an open epoch with a
/// path that never opened it, or that opened and closed an unrelated earlier
/// epoch, is unambiguous - `CLOSEUPVALS` detaches the earlier box, so the
/// register behaves exactly as if it had never been captured on that path.
fn validate_local_merge_openness(
    function: &Function,
    nodes: &[NodeIndex],
    exit_states: &FxHashMap<NodeIndex, OpenState>,
    old_locals: &FxHashMap<ast::RcLocal, ast::RcLocal>,
    site_epochs: &SiteEpochs,
    epochs: &mut EpochForest,
    local: &ast::RcLocal,
) -> Result<(), UpvalueAnalysisError> {
    let mut detached = FxHashMap::default();
    let mut open_epochs = Vec::new();
    for &node in nodes {
        let mut predecessor_count = 0usize;
        let mut open_count = 0usize;
        let mut first_epoch = None;
        let mut distinct_epoch = false;
        open_epochs.clear();
        for predecessor in function.predecessor_blocks(node) {
            let Some(state) = exit_states.get(&predecessor) else {
                continue;
            };
            predecessor_count = predecessor_count.checked_add(1).ok_or_else(|| {
                UpvalueAnalysisError::Resource("predecessor count overflow".into())
            })?;
            if let Some(epoch) = state.get(local) {
                open_count = open_count.checked_add(1).ok_or_else(|| {
                    UpvalueAnalysisError::Resource("merge presence count overflow".into())
                })?;
                if let Some(first_epoch) = first_epoch {
                    distinct_epoch |= first_epoch != *epoch;
                } else {
                    first_epoch = Some(*epoch);
                }
                let canonical = epochs.find(*epoch);
                if !open_epochs.contains(&canonical) {
                    reserve(open_epochs.try_reserve(1))?;
                    open_epochs.push(canonical);
                }
            }
        }
        if predecessor_count <= 1
            || open_count == 0
            || (open_count == predecessor_count && !distinct_epoch)
        {
            continue;
        }

        for index in 0..open_epochs.len() {
            let epoch = open_epochs[index];
            if !detached.contains_key(&epoch) {
                let flags = epoch_detachment(
                    function,
                    nodes,
                    old_locals,
                    site_epochs,
                    epochs,
                    local,
                    epoch,
                )?;
                reserve(detached.try_reserve(1))?;
                detached.insert(epoch, flags);
            }
            let flags = &detached[&epoch];
            for predecessor in function.predecessor_blocks(node) {
                if !exit_states.contains_key(&predecessor) {
                    continue;
                }
                if flags
                    .get(predecessor.index())
                    .is_some_and(|flags| flags & EPOCH_DETACHED != 0)
                {
                    return Err(UpvalueAnalysisError::PathDependentMerge {
                        block: node.index(),
                    });
                }
            }
        }
    }
    Ok(())
}

const EPOCH_LIVE: u8 = 1;
const EPOCH_DETACHED: u8 = 2;

/// Tracks one capture epoch of `tracked_local` through the graph.
///
/// `EPOCH_LIVE` marks block exits where the epoch's box is still attached to
/// the register on some path, `EPOCH_DETACHED` marks block exits where some
/// path has already closed it. Both are may-properties, so a block exit can
/// carry either, both, or neither.
fn epoch_detachment(
    function: &Function,
    nodes: &[NodeIndex],
    old_locals: &FxHashMap<ast::RcLocal, ast::RcLocal>,
    site_epochs: &SiteEpochs,
    epochs: &mut EpochForest,
    tracked_local: &ast::RcLocal,
    tracked_epoch: EpochId,
) -> Result<Vec<u8>, UpvalueAnalysisError> {
    let mut state_count = 0usize;
    for node in function.graph().node_indices() {
        let bound = node
            .index()
            .checked_add(1)
            .ok_or_else(|| UpvalueAnalysisError::Resource("node index overflow".into()))?;
        state_count = state_count.max(bound);
    }
    let mut exit_flags = Vec::new();
    reserve(exit_flags.try_reserve_exact(state_count))?;
    exit_flags.resize(state_count, 0u8);
    let mut worklist = VecDeque::new();
    reserve(worklist.try_reserve(nodes.len()))?;
    let mut queued = FxHashSet::default();
    reserve(queued.try_reserve(nodes.len()))?;
    for &node in nodes {
        worklist.push_back(node);
        queued.insert(node);
    }

    while let Some(node) = worklist.pop_front() {
        queued.remove(&node);
        let mut flags = 0u8;
        for predecessor in function.predecessor_blocks(node) {
            if let Some(predecessor_flags) = exit_flags.get(predecessor.index()) {
                flags |= predecessor_flags;
            }
        }
        for (statement_index, statement) in function.block(node).unwrap().iter().enumerate() {
            let mut opens_tracked_epoch = None;
            for_each_reference_capture(statement, old_locals, |local| {
                if &local == tracked_local {
                    let site_epoch = site_epochs[&(local, node, statement_index)];
                    opens_tracked_epoch = Some(epochs.find(site_epoch) == tracked_epoch);
                }
            });
            // A capture makes the site's epoch the only one attached to the
            // register, so any other epoch stops being live here.
            if let Some(opens_tracked_epoch) = opens_tracked_epoch {
                flags = (flags & EPOCH_DETACHED) | if opens_tracked_epoch { EPOCH_LIVE } else { 0 };
            }
            if let ast::Statement::Close(close) = statement
                && close.locals.contains(tracked_local)
            {
                if flags & EPOCH_LIVE != 0 {
                    flags |= EPOCH_DETACHED;
                }
                flags &= !EPOCH_LIVE;
            }
        }
        if exit_flags[node.index()] != flags {
            exit_flags[node.index()] = flags;
            for successor in function.successor_blocks(node) {
                if queued.insert(successor) {
                    worklist.push_back(successor);
                }
            }
        }
    }

    Ok(exit_flags)
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
    let additional = block_capture_count(block)?;
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
) -> Result<FxHashMap<NodeIndex, FxHashMap<ast::RcLocal, OpenRanges>>, UpvalueAnalysisError> {
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
        let capture_count = block_capture_count(block)?;
        reserve(state.try_reserve(capture_count))?;
        let mut capture_counts = FxHashMap::default();
        reserve(capture_counts.try_reserve(capture_count))?;
        for value in block.iter() {
            for_each_reference_capture(value, old_locals, |local| {
                let count = capture_counts.entry(local).or_insert(0usize);
                *count = count.checked_add(1).expect("bounded by capture_count");
            });
        }
        let range_capacity = state
            .len()
            .checked_add(capture_counts.len())
            .ok_or_else(|| UpvalueAnalysisError::Resource("range state count overflow".into()))?;
        let mut block_opened = FxHashMap::default();
        reserve(block_opened.try_reserve(range_capacity))?;
        for (local, count) in capture_counts {
            let capacity = count
                .checked_add(usize::from(state.contains_key(&local)))
                .ok_or_else(|| {
                    UpvalueAnalysisError::Resource("local range count overflow".into())
                })?;
            block_opened.insert(local, OpenRanges::with_capacity(capacity)?);
        }
        for (local, epoch) in &state {
            if let Some(ranges) = block_opened.get_mut(local) {
                ranges.push(0..=end, epochs.site(*epoch));
            } else {
                let mut ranges = OpenRanges::with_capacity(1)?;
                ranges.push(0..=end, epochs.site(*epoch));
                block_opened.insert(local.clone(), ranges);
            }
        }

        for (statement, value) in block.iter().enumerate() {
            for_each_reference_capture(value, old_locals, |local| {
                let site_epoch = site_epochs[&(local.clone(), node, statement)];
                if let Some(active) = state.get(&local).copied() {
                    state.insert(local, epochs.union(active, site_epoch));
                } else {
                    state.insert(local.clone(), site_epoch);
                    block_opened
                        .get_mut(&local)
                        .expect("capture range capacity was reserved")
                        .push(statement..=end, epochs.site(site_epoch));
                }
            });
            if let ast::Statement::Close(close) = value {
                for local in &close.locals {
                    state.remove(local);
                    if let Some(ranges) = block_opened.get_mut(local) {
                        ranges.close_from(statement);
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

        let analysis = UpvaluesOpen::try_new(&function, old_locals).unwrap();
        let maximum_locations = analysis
            .open
            .values()
            .flat_map(|locals| locals.values())
            .flat_map(|ranges| ranges.0.iter().map(|_| 1))
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
    fn never_captured_path_does_not_conflict_with_open_path() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let captures = function.new_block();
        let bypasses = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function
            .block_mut(captures)
            .unwrap()
            .push(capture(&captured));
        function.block_mut(bypasses).unwrap().push(marker());
        function.block_mut(merge).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, captures, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, bypasses, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(captures, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(bypasses, merge, BlockEdge::default());

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(
            analysis.opening_location(merge, &captured, 0),
            Some((captures, 0))
        );
    }

    #[test]
    fn closed_epoch_does_not_conflict_with_a_later_open_epoch() {
        // An inlined helper whose body conditionally captures its parameter is
        // emitted twice into the same register: the first epoch is closed
        // before the second one opens, so the merge after the second `if` sees
        // one predecessor holding the second epoch open and one predecessor on
        // which nothing is attached to the register any more.
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let captures_first = function.new_block();
        let closes = function.new_block();
        let captures_second = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(marker());
        function
            .block_mut(captures_first)
            .unwrap()
            .push(capture(&captured));
        function.block_mut(closes).unwrap().push(
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
        );
        function
            .block_mut(captures_second)
            .unwrap()
            .push(capture(&captured));
        function.block_mut(merge).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, captures_first, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, closes, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(captures_first, closes, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(closes, captures_second, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(closes, merge, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(captures_second, merge, BlockEdge::default());

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(
            analysis.opening_location(merge, &captured, 0),
            Some((captures_second, 0))
        );
    }

    #[test]
    fn closed_epoch_still_conflicts_with_the_same_open_epoch() {
        // Same shape as above, except the *second* epoch is the one closed on
        // only one path. Whether writes after the merge reach the closure built
        // at that epoch's capture site is path dependent, so the earlier
        // already-closed epoch must not excuse it.
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let captures_first = function.new_block();
        let closes_first = function.new_block();
        let captures_second = function.new_block();
        let closes_second = function.new_block();
        let keeps_open = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(marker());
        function
            .block_mut(captures_first)
            .unwrap()
            .push(capture(&captured));
        function.block_mut(closes_first).unwrap().push(
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
        );
        function
            .block_mut(captures_second)
            .unwrap()
            .push(capture(&captured));
        function.block_mut(closes_second).unwrap().push(
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
        );
        function.block_mut(keeps_open).unwrap().push(marker());
        function.block_mut(merge).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, captures_first, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, closes_first, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(captures_first, closes_first, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(closes_first, captures_second, BlockEdge::default());
        function.graph_mut().add_edge(
            captures_second,
            closes_second,
            BlockEdge::new(BranchType::Then),
        );
        function.graph_mut().add_edge(
            captures_second,
            keeps_open,
            BlockEdge::new(BranchType::Else),
        );
        function
            .graph_mut()
            .add_edge(closes_second, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(keeps_open, merge, BlockEdge::default());

        let error = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap_err();

        assert!(matches!(
            error,
            super::UpvalueAnalysisError::PathDependentMerge { block }
                if block == merge.index()
        ));
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

    #[test]
    fn unreachable_predecessor_does_not_open_capture_at_reachable_merge() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let merge = function.new_block();
        let unreachable = function.new_block();
        function.set_entry(entry);
        function.block_mut(merge).unwrap().push(marker());
        function
            .block_mut(unreachable)
            .unwrap()
            .push(capture(&captured));
        function
            .graph_mut()
            .add_edge(entry, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(unreachable, merge, BlockEdge::default());

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(analysis.opening_location(merge, &captured, 0), None);
    }

    #[test]
    fn conditional_close_is_rejected_at_reachable_merge() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let closes = function.new_block();
        let keeps_open = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(capture(&captured));
        function.block_mut(closes).unwrap().push(
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
        );
        function.block_mut(keeps_open).unwrap().push(marker());
        function.block_mut(merge).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, closes, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, keeps_open, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(closes, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(keeps_open, merge, BlockEdge::default());

        let error = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap_err();

        assert!(matches!(
            error,
            super::UpvalueAnalysisError::PathDependentMerge { block }
                if block == merge.index()
        ));
    }

    #[test]
    fn conditional_close_and_reopen_is_rejected_at_reachable_merge() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        let reopens = function.new_block();
        let keeps_open = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(capture(&captured));
        function.block_mut(reopens).unwrap().extend([
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
            capture(&captured),
        ]);
        function.block_mut(keeps_open).unwrap().push(marker());
        function.block_mut(merge).unwrap().push(marker());
        function
            .graph_mut()
            .add_edge(entry, reopens, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, keeps_open, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(reopens, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(keeps_open, merge, BlockEdge::default());

        let error = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap_err();

        assert!(matches!(
            error,
            super::UpvalueAnalysisError::PathDependentMerge { block }
                if block == merge.index()
        ));
    }

    #[test]
    fn close_and_reopen_create_disjoint_ranges_in_one_block() {
        let captured = RcLocal::default();
        let mut function = Function::new(0);
        let entry = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().extend([
            capture(&captured),
            marker(),
            Close {
                locals: vec![captured.clone()],
            }
            .into(),
            marker(),
            capture(&captured),
            marker(),
        ]);

        let analysis = UpvaluesOpen::try_new(&function, identity_map(&captured)).unwrap();

        assert_eq!(
            analysis.opening_location(entry, &captured, 0),
            Some((entry, 0))
        );
        assert_eq!(
            analysis.opening_location(entry, &captured, 1),
            Some((entry, 0))
        );
        assert_eq!(analysis.opening_location(entry, &captured, 2), None);
        assert_eq!(analysis.opening_location(entry, &captured, 3), None);
        assert_eq!(
            analysis.opening_location(entry, &captured, 4),
            Some((entry, 4))
        );
        assert_eq!(
            analysis.opening_location(entry, &captured, 5),
            Some((entry, 4))
        );
    }
}
