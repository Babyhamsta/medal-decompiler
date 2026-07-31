use std::iter;

use ast::{LocalRw, RcLocal, Traverse};
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use petgraph::{
    Direction,
    stable_graph::NodeIndex,
    visit::{Dfs, EdgeRef, Walker},
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{function::Function, ssa::param_dependency_graph::ParamDependencyGraph};

use super::{SsaError, upvalues::UpvaluesOpen};

struct SsaConstructor<'a> {
    function: &'a mut Function,
    dfs: IndexSet<NodeIndex>,
    incomplete_params: FxHashMap<NodeIndex, FxHashMap<RcLocal, RcLocal>>,
    filled_blocks: FxHashSet<NodeIndex>,
    sealed_blocks: FxHashSet<NodeIndex>,
    // TODO: combine current/all/old into one map
    current_definition: FxHashMap<RcLocal, FxHashMap<NodeIndex, RcLocal>>,
    all_definitions: FxHashMap<RcLocal, FxHashSet<RcLocal>>,
    old_locals: FxHashMap<RcLocal, RcLocal>,
    local_count: usize,
    local_map: FxHashMap<RcLocal, RcLocal>,
    new_upvalues_in: IndexMap<RcLocal, FxHashSet<RcLocal>>,
    upvalues_passed: FxHashMap<RcLocal, FxHashMap<(NodeIndex, usize), FxHashSet<RcLocal>>>,
}

// TODO: REFACTOR: move out of construct module
// TODO: support RValues other than Local and use an local -> rvalue map
// https://github.com/fkie-cad/dewolf/blob/7afe5b46e79a7b56e9904e63f29d54bd8f7302d9/decompiler/pipeline/ssa/phi_cleaner.py
pub fn remove_unnecessary_params(
    function: &mut Function,
    local_map: &mut FxHashMap<RcLocal, RcLocal>,
) -> bool {
    let mut changed = false;
    for node in function.blocks().map(|(i, _)| i).collect::<Vec<_>>() {
        let mut dependency_graph = ParamDependencyGraph::new(function, node);
        let mut removable_params = FxHashMap::default();
        let edges = function
            .graph()
            .edges_directed(node, Direction::Incoming)
            .collect::<Vec<_>>();
        if !edges.is_empty() {
            let params = edges[0].weight().arguments.iter().map(|(p, _)| p);
            let args_in_by_block = edges
                .iter()
                .map(|e| {
                    e.weight()
                        .arguments
                        .iter()
                        .map(|(_, a)| a)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let mut params_to_remove = FxHashSet::default();
            for (index, mut param) in params.enumerate() {
                if args_in_by_block
                    .iter()
                    .map(|a| a[index])
                    .any(|r| r.as_local().is_none())
                {
                    continue;
                }
                // TODO: should we really be doing this by index?
                let arg_set = args_in_by_block
                    .iter()
                    .map(|a| a[index])
                    .filter_map(|r| r.as_local())
                    .collect::<FxHashSet<_>>();
                if arg_set.len() == 1 {
                    while let Some(param_to) = local_map.get(param) {
                        param = param_to;
                    }
                    let mut arg = arg_set.into_iter().next().unwrap();
                    while let Some(arg_to) = local_map.get(arg) {
                        arg = arg_to;
                    }
                    if arg != param {
                        // param is not trivial, we must replace the param with the arg
                        // y = phi(x, x, ..., x)
                        removable_params.insert(param.clone(), arg.clone());
                    } else {
                        // param is trivial
                        // x = phi(x, x, ..., x)
                        if let Some(&param_node) = dependency_graph.local_to_node.get(param) {
                            dependency_graph.remove_node(param_node);
                        }
                    }

                    params_to_remove.insert(param.clone());
                }
            }
            if !params_to_remove.is_empty() {
                for edge in edges.into_iter().map(|e| e.id()).collect::<Vec<_>>() {
                    function
                        .graph_mut()
                        .edge_weight_mut(edge)
                        .unwrap()
                        .arguments
                        .retain(|(p, _)| {
                            let mut p = p;
                            while let Some(p_to) = local_map.get(p) {
                                p = p_to;
                            }
                            !params_to_remove.contains(p)
                        });
                }
                changed = true;
            }
        }

        let mut removable_params_degree_zero = removable_params
            .iter()
            .map(|(p, a)| (p.clone(), a))
            .filter(|(p, _)| {
                dependency_graph
                    .graph
                    .neighbors(dependency_graph.local_to_node[p])
                    .count()
                    == 0
            })
            .collect::<Vec<_>>();

        while let Some((param, mut arg)) = removable_params_degree_zero.pop() {
            let param_node = dependency_graph.local_to_node[&param];
            for param_pred_node in dependency_graph
                .graph
                .neighbors_directed(param_node, Direction::Incoming)
            {
                if dependency_graph.graph.neighbors(param_pred_node).count() == 1 {
                    let param_pred = dependency_graph
                        .graph
                        .node_weight(param_pred_node)
                        .unwrap()
                        .clone();
                    if let Some(param_pred_arg) = removable_params.get(&param_pred) {
                        removable_params_degree_zero.push((param_pred, param_pred_arg));
                    }
                }
            }
            dependency_graph.remove_node(param_node);

            while let Some(arg_to) = local_map.get(arg) {
                arg = arg_to;
            }
            local_map.insert(param, arg.clone());
            changed = true;
        }
    }
    changed
}

// TODO: STYLE: rename function
// TODO: STYLE: rename `uses_local`, we need a generic name for ast nodes, maybe `traversible`?
fn apply_local_map_to_values_referenced<T: LocalRw + Traverse>(
    uses_local: &mut T,
    local_map: &FxHashMap<RcLocal, RcLocal>,
) {
    // TODO: figure out values_mut
    for (from, mut to) in uses_local
        .values_written_mut()
        .into_iter()
        .filter_map(|v| local_map.get(v).map(|t| (v, t)))
    {
        while let Some(to_to) = local_map.get(to) {
            to = to_to;
        }
        *from = to.clone();
    }
    let mut map = FxHashMap::default();
    for (from, mut to) in uses_local
        .values_read_mut()
        .into_iter()
        .filter_map(|v| local_map.get(v).map(|t| (v, t)))
    {
        while let Some(to_to) = local_map.get(to) {
            to = to_to;
        }
        map.insert(from.clone(), to.clone());
        *from = to.clone();
    }
    // uses_local.traverse_rvalues(&mut |rvalue| {
    //     if let Some(closure) = rvalue.as_closure_mut() {
    //         replace_locals(&mut closure.body, &map)
    //     }
    // });
}

fn resolve_local_map(local_map: &FxHashMap<RcLocal, RcLocal>, local: &RcLocal) -> RcLocal {
    let Some(mut target) = local_map.get(local) else {
        return local.clone();
    };
    while let Some(next) = local_map.get(target) {
        target = next;
    }
    target.clone()
}

/// Renames the members of by-reference capture groups through `local_map`.
///
/// A capture group is the set of SSA values that alias one runtime box, so a
/// pass that renames a value has to rename it inside the group as well.
/// Leaving a stale name behind drops the value out of its group: SSA
/// destruction then gives it a local of its own, and a write that the closure
/// is supposed to observe silently lands somewhere else. Callers must invoke
/// this alongside every [`apply_local_map`] that runs between capture marking
/// and SSA destruction.
pub fn apply_local_map_to_upvalue_groups(
    upvalue_to_group: &mut IndexMap<RcLocal, RcLocal>,
    local_map: &FxHashMap<RcLocal, RcLocal>,
) {
    if local_map.is_empty() || upvalue_to_group.is_empty() {
        return;
    }
    let mut renamed = IndexMap::with_capacity(upvalue_to_group.len());
    for (upvalue, group) in upvalue_to_group.iter() {
        let target = resolve_local_map(local_map, upvalue);
        // Members of one group collapsing onto a single local is expected;
        // members of *different* groups doing so would mean two distinct
        // boxes were merged, which the renaming passes never do.
        debug_assert!(
            renamed
                .get(&target)
                .is_none_or(|existing: &RcLocal| existing == group),
            "capture groups must not collapse onto one local"
        );
        renamed.entry(target).or_insert_with(|| group.clone());
    }
    *upvalue_to_group = renamed;
}

// does not replace locals in child closures
pub fn apply_local_map(function: &mut Function, local_map: FxHashMap<RcLocal, RcLocal>) {
    let mut provenance_groups = FxHashMap::<RcLocal, FxHashSet<RcLocal>>::default();
    for source in local_map.keys() {
        let mut target = &local_map[source];
        while let Some(next) = local_map.get(target) {
            target = next;
        }
        provenance_groups
            .entry(target.clone())
            .or_default()
            .extend([source.clone(), target.clone()]);
    }
    for (target, sources) in provenance_groups {
        function.merge_local_provenance(target, sources.iter());
    }

    for param in &mut function.parameters {
        if let Some(mut new_param) = local_map.get(param) {
            // TODO: make sure this doesnt cycle if theres a li -> li entry
            while let Some(new_to) = local_map.get(new_param) {
                new_param = new_to;
            }
            *param = new_param.clone();
        }
    }
    // TODO: blocks_mut
    for node in function.graph().node_indices().collect::<Vec<_>>() {
        let block = function.block_mut(node).unwrap();
        for stat in block.iter_mut() {
            apply_local_map_to_values_referenced(stat, &local_map);
        }
        for edge in function.edges(node).map(|e| e.id()).collect::<Vec<_>>() {
            // TODO: rename Stat::values, Expr::values to locals() and refer to locals as locals everywhere
            for local in function
                .graph_mut()
                .edge_weight_mut(edge)
                .unwrap()
                .arguments
                .iter_mut()
                .flat_map(|(p, a)| iter::once(Either::Left(p)).chain(iter::once(Either::Right(a))))
            {
                match local {
                    Either::Left(local) => {
                        if let Some(mut new_local) = local_map.get(local) {
                            // TODO: make sure this doesnt cycle if theres a li -> li entry
                            // also see TODO in destruct.rs
                            while let Some(new_to) = local_map.get(new_local) {
                                new_local = new_to;
                            }
                            *local = new_local.clone();
                        }
                    }
                    Either::Right(rvalue) => {
                        apply_local_map_to_values_referenced(rvalue, &local_map);
                    }
                }
            }
        }
    }
}

// based on "Simple and Efficient Construction of Static Single Assignment Form" (https://pp.info.uni-karlsruhe.de/uploads/publikationen/braun13cc.pdf)
impl<'a> SsaConstructor<'a> {
    fn write_local(&mut self, node: NodeIndex, local: &RcLocal, new_local: &RcLocal) {
        self.all_definitions
            .entry(local.clone())
            .or_default()
            .insert(new_local.clone());
        self.current_definition
            .entry(local.clone())
            .or_default()
            .insert(node, new_local.clone());
    }

    fn add_param_args(
        &mut self,
        node: NodeIndex,
        local: &RcLocal,
        param_local: RcLocal,
    ) -> RcLocal {
        let mut argument_locals = Vec::new();
        for (source, edge) in self
            .function
            .graph()
            .edges_directed(node, Direction::Incoming)
            .map(|e| (e.source(), e.id()))
            .collect::<Vec<_>>()
        {
            let argument_local = self.find_local(source, local);
            argument_locals.push(argument_local.clone());
            self.function
                .graph_mut()
                .edge_weight_mut(edge)
                .unwrap()
                .arguments
                .push((param_local.clone(), argument_local.into()));
        }
        self.function
            .merge_local_provenance(param_local.clone(), argument_locals.iter());
        // TODO: fix lol
        // self.try_remove_trivial_param(node, param_local)
        param_local
    }

    fn try_remove_trivial_param(&mut self, node: NodeIndex, param_local: RcLocal) -> RcLocal {
        let mut same = None;
        let args_in = self.function.edges_to_block(node).map(|(_, e)| {
            &e.arguments
                .iter()
                .find(|(p, _)| p == &param_local)
                .unwrap()
                .1
        });
        for arg in args_in {
            let mut arg = arg.as_local().unwrap();
            while let Some(arg_to) = self.local_map.get(arg) {
                arg = arg_to;
            }

            if Some(&arg) == same.as_ref() || arg == &param_local {
                // unique value or self-reference
                continue;
            }
            if same.is_some() {
                // the param merges at least two values: not trivial
                return param_local;
            }
            same = Some(arg);
        }
        let same = same.unwrap().clone();
        self.local_map.insert(param_local.clone(), same.clone());

        // TODO: optimize
        for node in self.function.graph().node_indices().collect::<Vec<_>>() {
            let mut edges = self
                .function
                .edges_to_block(node)
                .map(|(_, e)| e)
                .peekable();
            if edges
                .peek()
                .map(|e| !e.arguments.is_empty())
                .unwrap_or(false)
            {
                let edges = edges.collect::<Vec<_>>();
                if edges.iter().any(|e| {
                    e.arguments
                        .iter()
                        .any(|(_, a)| a.as_local().unwrap() == &param_local)
                }) {
                    let params_in = edges
                        .into_iter()
                        .map(|e| {
                            e.arguments
                                .iter()
                                .map(|(p, _)| p)
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    for mut param in params_in[0].iter() {
                        while let Some(param_to) = self.local_map.get(param) {
                            param = param_to;
                        }

                        if param == &param_local
                            || params_in.iter().any(|e| e.iter().any(|p| p == param))
                        {
                            self.try_remove_trivial_param(node, param.clone());
                        }
                    }
                }
            }
        }

        same
    }

    fn find_local(&mut self, node: NodeIndex, local: &RcLocal) -> RcLocal {
        let res = if let Some(new_local) = self
            .current_definition
            .get(local)
            .and_then(|x| x.get(&node))
        {
            // local to block
            new_local.clone()
        } else {
            // search globally
            if !self.sealed_blocks.contains(&node) {
                // TODO: this code is repeated multiple times, create new_local function
                let param_local = self.function.new_merge_local(local);
                self.old_locals.insert(param_local.clone(), local.clone());
                if let Some(upvalues) = self.new_upvalues_in.get_mut(local) {
                    upvalues.insert(param_local.clone());
                }
                self.local_count += 1;
                self.incomplete_params
                    .entry(node)
                    .or_default()
                    .insert(local.clone(), param_local.clone());
                param_local
            } else if let Ok(pred) = self.function.predecessor_blocks(node).exactly_one() {
                self.find_local(pred, local)
            } else {
                let param_local = self.function.new_merge_local(local);
                self.old_locals.insert(param_local.clone(), local.clone());
                if let Some(upvalues) = self.new_upvalues_in.get_mut(local) {
                    upvalues.insert(param_local.clone());
                }
                self.local_count += 1;
                self.write_local(node, local, &param_local);

                self.add_param_args(node, local, param_local)
            }
        };
        self.write_local(node, local, &res);
        res
    }

    fn propagate_copies(&mut self) {
        // TODO: blocks_mut
        for node in self.function.graph().node_indices().collect::<Vec<_>>() {
            let block = self.function.block_mut(node).unwrap();
            for index in block
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_assign().map(|_| i))
                .collect::<Vec<_>>()
            {
                let block = self.function.block_mut(node).unwrap();
                let assign = block[index].as_assign().unwrap();
                if assign.left.len() == 1
                    && assign.right.len() == 1
                    && let Some(from) = assign.left[0].as_local()
                    && let from_old = &self.old_locals[from]
                    && !self.new_upvalues_in.contains_key(from_old)
                    && !self.upvalues_passed.contains_key(from_old)
                    && let Some(mut to) = assign.right[0].as_local()
                {
                    // TODO: STYLE: this name lol
                    while let Some(to_to) = self.local_map.get(to) {
                        to = to_to;
                    }
                    let to_old = &self.old_locals[to];
                    if !self.new_upvalues_in.contains_key(to_old)
                        && !self.upvalues_passed.contains_key(to_old)
                    {
                        self.local_map.insert(from.clone(), to.clone());
                        block[index] = ast::Empty {}.into();
                    }
                }
            }
            // we check block.ast.len() elsewhere and do `i - ` elsewhere so we need to get rid of empty statements
            // TODO: fix here and elsewhere, see inline.rs
            let block = self.function.block_mut(node).unwrap();
            block.retain(|s| s.as_empty().is_none());
        }
    }

    /// Keeps the recorded capture groups in step with a pending `local_map`.
    ///
    /// The renames applied after [`Self::mark_upvalues`] rewrite the function
    /// but not the groups, so a member that gets renamed would otherwise be
    /// left out of its group and lose its by-reference binding.
    fn rename_upvalues_passed(&mut self) {
        if self.local_map.is_empty() {
            return;
        }
        let local_map = &self.local_map;
        for sites in self.upvalues_passed.values_mut() {
            for members in sites.values_mut() {
                *members = members
                    .iter()
                    .map(|member| resolve_local_map(local_map, member))
                    .collect();
            }
        }
    }

    fn mark_upvalues(&mut self) -> Result<(), SsaError> {
        let upvalues_open = UpvaluesOpen::try_new(self.function, self.old_locals.clone())?;
        for &node in &self.dfs {
            for stat_index in 0..self.function.block(node).unwrap().len() {
                let statement = self.function.block(node).unwrap().get(stat_index).unwrap();
                let values = statement.values().into_iter().cloned().collect::<Vec<_>>();
                for value in values {
                    let old_local = &self.old_locals[&value];
                    if let Some(opening_location) =
                        upvalues_open.opening_location(node, old_local, stat_index)
                    {
                        if let Some(new_upvalues_in) = self.new_upvalues_in.get_mut(old_local) {
                            assert!(new_upvalues_in.contains(&value));
                        } else {
                            self.upvalues_passed
                                .entry(old_local.clone())
                                .or_default()
                                .entry(opening_location)
                                .or_default()
                                .insert(value);
                        }
                    }
                }
            }
            self.function
                .block_mut(node)
                .unwrap()
                .retain(|statement| !matches!(statement, ast::Statement::Close(_)))
        }
        Ok(())
    }

    fn read(&mut self, node: NodeIndex, stat_index: usize) {
        let statement = self
            .function
            .block_mut(node)
            .unwrap()
            .get_mut(stat_index)
            .unwrap();
        let read = statement
            .values_read()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        // TODO: do we need two loops?
        let mut map = FxHashMap::default();
        map.reserve(read.len());
        // TODO: REFACTOR: extend
        for local in &read {
            let new_local = self.find_local(node, local);
            self.function.record_use(&new_local, node, stat_index);
            map.insert(local.clone(), new_local);
        }
        for (local_index, local) in read.into_iter().enumerate() {
            let statement = self
                .function
                .block_mut(node)
                .unwrap()
                .get_mut(stat_index)
                .unwrap();
            *statement.values_read_mut()[local_index] = map[&local].clone();
        }
    }

    fn construct(
        mut self,
    ) -> Result<
        (
            usize,
            Vec<FxHashSet<RcLocal>>,
            Vec<(RcLocal, FxHashSet<RcLocal>)>,
            Vec<FxHashSet<RcLocal>>,
        ),
        SsaError,
    > {
        let entry = self.function.entry().unwrap();
        let mut visited_nodes = Vec::with_capacity(self.function.graph().node_count());
        for i in 0..self.dfs.len() {
            let node = self.dfs[i];
            visited_nodes.push(node);
            for stat_index in 0..self.function.block(node).unwrap().len() {
                let statement = self
                    .function
                    .block_mut(node)
                    .unwrap()
                    .get_mut(stat_index)
                    .unwrap();
                if let Some(assign) = statement.as_assign()
                    && assign.left.len() == 1
                    && assign.right.len() == 1
                    && let Some(local) = assign.left[0].as_local().cloned()
                    && assign.right[0].as_closure().is_some()
                {
                    let new_local = self.function.new_ssa_definition(&local, node, stat_index);
                    self.old_locals.insert(new_local.clone(), local.clone());
                    if let Some(upvalues) = self.new_upvalues_in.get_mut(&local) {
                        upvalues.insert(new_local.clone());
                    }
                    self.local_count += 1;
                    self.write_local(node, &local, &new_local);
                    let statement = self
                        .function
                        .block_mut(node)
                        .unwrap()
                        .get_mut(stat_index)
                        .unwrap();
                    let assign = statement.as_assign_mut().unwrap();
                    *assign.left[0].as_local_mut().unwrap() = new_local.clone();
                    // we do read after bc of recursive closures
                    self.read(node, stat_index);
                } else {
                    let written = statement
                        .values_written()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>();
                    self.read(node, stat_index);
                    // write
                    for (local_index, local) in written.iter().enumerate() {
                        let new_local = self.function.new_ssa_definition(local, node, stat_index);
                        self.old_locals.insert(new_local.clone(), local.clone());
                        if let Some(upvalues) = self.new_upvalues_in.get_mut(local) {
                            upvalues.insert(new_local.clone());
                        }
                        self.local_count += 1;
                        self.write_local(node, local, &new_local);
                        let statement = self
                            .function
                            .block_mut(node)
                            .unwrap()
                            .get_mut(stat_index)
                            .unwrap();
                        *statement.values_written_mut()[local_index] = new_local;
                    }
                }

                // if !map.is_empty() {
                //     let statement = self
                //         .function
                //         .block_mut(node)
                //         .unwrap()
                //         .get_mut(stat_index)
                //         .unwrap();
                //     statement.traverse_rvalues(&mut |rvalue| {
                //         if let Some(closure) = rvalue.as_closure_mut() {
                //             replace_locals(&mut closure.body, &map)
                //         }
                //     });
                // }
            }
            self.filled_blocks.insert(node);

            for &node in &visited_nodes {
                if node != entry
                    && !self.sealed_blocks.contains(&node)
                    && !self
                        .function
                        .predecessor_blocks(node)
                        .any(|p| !self.filled_blocks.contains(&p))
                {
                    if let Some(incomplete_params) = self.incomplete_params.remove(&node) {
                        for (local, param_local) in incomplete_params {
                            // TODO: this is a bit weird, maybe we should have a upvalue rvalue
                            if !self.new_upvalues_in.contains_key(&local) {
                                self.add_param_args(node, &local, param_local);
                            }
                        }
                    }
                    self.sealed_blocks.insert(node);
                }
            }
        }

        // TODO: this is a bit meh, maybe we should have an argument rvalue
        if let Some(mut incomplete_params) = self.incomplete_params.remove(&entry) {
            for param in &mut self.function.parameters {
                *param = incomplete_params.remove(param).unwrap_or_default();
            }
        }
        assert!(self.incomplete_params.is_empty());

        // TODO: irreducible control flow (see the paper this algorithm is from)
        // TODO: apply_local_map unnecessary number of calls
        apply_local_map(self.function, std::mem::take(&mut self.local_map));

        self.mark_upvalues()?;
        self.propagate_copies();
        self.rename_upvalues_passed();
        apply_local_map(self.function, std::mem::take(&mut self.local_map));

        // TODO: loop until returns false?
        remove_unnecessary_params(self.function, &mut self.local_map);

        for (derived, original) in &self.old_locals {
            let original_name = original.0.0.lock().0.clone();
            if let Some(name) =
                original_name.filter(|name| ast::is_valid_identifier(name.as_bytes()))
            {
                let mut derived = derived.0.0.lock();
                if derived.0.is_none() {
                    derived.0 = Some(name);
                }
            }
        }

        let mut coalesced_names = FxHashMap::<RcLocal, FxHashSet<String>>::default();
        for (source, target) in &self.local_map {
            let mut target = target;
            while let Some(next) = self.local_map.get(target) {
                target = next;
            }
            if let Some(name) = source
                .0
                .0
                .lock()
                .0
                .clone()
                .filter(|name| ast::is_valid_identifier(name.as_bytes()))
            {
                coalesced_names
                    .entry(target.clone())
                    .or_default()
                    .insert(name);
            }
        }
        for (target, mut names) in coalesced_names {
            if let Some(name) = target
                .0
                .0
                .lock()
                .0
                .clone()
                .filter(|name| ast::is_valid_identifier(name.as_bytes()))
            {
                names.insert(name);
            }
            target.0.0.lock().0 = (names.len() == 1).then(|| names.into_iter().next().unwrap());
        }

        self.rename_upvalues_passed();
        apply_local_map(self.function, std::mem::take(&mut self.local_map));

        Ok((
            self.local_count,
            self.all_definitions.into_values().collect(),
            self.new_upvalues_in.into_iter().collect(),
            self.upvalues_passed
                .into_values()
                .flat_map(|m| m.into_values())
                .collect(),
        ))
    }
}

pub fn construct(
    function: &mut Function,
    upvalues_in: &Vec<RcLocal>,
) -> Result<
    (
        usize,
        Vec<FxHashSet<RcLocal>>,
        Vec<(RcLocal, FxHashSet<RcLocal>)>,
        Vec<FxHashSet<RcLocal>>,
    ),
    SsaError,
> {
    // An entry block with predecessors may never be marked complete, which
    // would leave its block parameters unresolved.
    // TODO: verify ^ and insert temporary entry that's removed if there is no block params (if its an issue)
    assert!(
        function
            .predecessor_blocks(function.entry().unwrap())
            .next()
            .is_none()
    );
    let mut new_upvalues_in = IndexMap::with_capacity(upvalues_in.len());
    for upvalue in upvalues_in {
        new_upvalues_in.insert(upvalue.clone(), FxHashSet::default());
    }

    let dfs = Dfs::new(function.graph(), function.entry().unwrap())
        .iter(function.graph())
        .collect::<IndexSet<_>>();

    // remove all nodes that will never execute
    for node in function.blocks().map(|(n, _)| n).collect::<Vec<_>>() {
        if !dfs.contains(&node) {
            function.remove_block(node);
        }
    }
    let node_count = function.graph().node_count();
    SsaConstructor {
        function,
        dfs,
        incomplete_params: FxHashMap::with_capacity_and_hasher(node_count, Default::default()),
        filled_blocks: FxHashSet::with_capacity_and_hasher(node_count, Default::default()),
        sealed_blocks: FxHashSet::with_capacity_and_hasher(node_count, Default::default()),
        current_definition: FxHashMap::default(),
        all_definitions: FxHashMap::default(),
        old_locals: FxHashMap::default(),
        local_count: 0,
        local_map: FxHashMap::default(),
        new_upvalues_in,
        upvalues_passed: FxHashMap::default(),
    }
    .construct()
}

#[cfg(test)]
mod provenance_tests {
    use ast::{Assign, Literal, Local, RcLocal};
    use rustc_hash::FxHashMap;

    use crate::{
        function::Function,
        provenance::{BindingIdentity, DebugLifetime, RegisterFamily, SourceOrigin},
    };

    use super::apply_local_map;

    #[test]
    fn ssa_destruction_coalescing_retains_all_definition_origins() {
        let physical = RcLocal::new(Local::default());
        let mut function = Function::new(4);
        let node = function.new_block();
        function.set_entry(node);
        function.block_mut(node).unwrap().extend([
            Assign::new(
                vec![physical.clone().into()],
                vec![Literal::Number(1.0).into()],
            )
            .into(),
            Assign::new(
                vec![physical.clone().into()],
                vec![Literal::Number(2.0).into()],
            )
            .into(),
        ]);
        function.set_statement_origins(
            node,
            vec![
                [SourceOrigin::new(4, 2, Some(10), "LOADN")].into(),
                [SourceOrigin::new(4, 8, Some(20), "LOADN")].into(),
            ],
        );
        function.set_register_family(
            physical.clone(),
            RegisterFamily::new(
                4,
                1,
                BindingIdentity::local(4, 1),
                vec![
                    DebugLifetime::new(b"first".to_vec(), 1, 4),
                    DebugLifetime::new(b"second".to_vec(), 7, 10),
                ],
            ),
        );

        let first = function.new_ssa_definition(&physical, node, 0);
        let second = function.new_ssa_definition(&physical, node, 1);
        apply_local_map(
            &mut function,
            FxHashMap::from_iter([(second, first.clone())]),
        );

        assert_eq!(function.provenance().origins(&first).len(), 2);
        assert_eq!(function.provenance().bindings(&first).len(), 2);
    }
}

#[cfg(test)]
mod upvalue_group_tests {
    use ast::{Assign, Binary, BinaryOperation, Closure, Literal, RcLocal, Return, Upvalue};
    use indexmap::IndexMap;
    use rustc_hash::FxHashMap;

    use crate::{
        block::{BlockEdge, BranchType},
        function::Function,
        provenance::{BindingIdentity, SourceOrigin},
    };

    use super::{apply_local_map, apply_local_map_to_upvalue_groups, remove_unnecessary_params};

    #[test]
    fn renaming_rewrites_group_members_and_follows_chains() {
        let group = RcLocal::default();
        let first = RcLocal::default();
        let second = RcLocal::default();
        let third = RcLocal::default();
        let survivor = RcLocal::default();
        let mut upvalue_to_group = IndexMap::from_iter([
            (first.clone(), group.clone()),
            (second.clone(), group.clone()),
            (third.clone(), group.clone()),
        ]);

        apply_local_map_to_upvalue_groups(
            &mut upvalue_to_group,
            // `first -> second -> survivor` has to be followed all the way,
            // and `third` collapsing onto `survivor` too must not drop it.
            &FxHashMap::from_iter([
                (first, second.clone()),
                (second, survivor.clone()),
                (third, survivor.clone()),
            ]),
        );

        let expected: IndexMap<RcLocal, RcLocal> = IndexMap::from_iter([(survivor, group)]);
        assert_eq!(
            upvalue_to_group, expected,
            "every renamed member must land on its group under its new name"
        );
    }

    /// A register captured by reference on one path and written after the
    /// merge must stay in one capture group across the renames the structuring
    /// passes perform.
    ///
    /// This is the SSA form the pipeline reaches once the conditional has been
    /// structured: the closure captures a block parameter, and collapsing the
    /// redundant parameters rewrites that member onto the entry definition -
    /// a local that was never a group member. Leaving the group keyed on the
    /// old names drops the captured value out of it, SSA destruction then gives
    /// the write a local of its own, and the closure keeps observing the value
    /// from before the write.
    #[test]
    fn write_after_conditional_capture_stays_in_the_capture_group() {
        let entry_value = RcLocal::default();
        let capture_param = RcLocal::default();
        let merge_param = RcLocal::default();
        let written = RcLocal::default();
        let closure_slot = RcLocal::default();
        let group = RcLocal::default();

        let mut function = Function::new(0);
        for (index, local) in [
            &entry_value,
            &capture_param,
            &merge_param,
            &written,
            &closure_slot,
            &group,
        ]
        .into_iter()
        .enumerate()
        {
            function.set_binding(local.clone(), BindingIdentity::local(0, index));
        }

        let entry = function.new_block();
        let captures = function.new_block();
        let merge = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(
            Assign::new(
                vec![entry_value.clone().into()],
                vec![Literal::String(b"a".to_vec().into()).into()],
            )
            .into(),
        );
        function.block_mut(captures).unwrap().push(
            Assign::new(
                vec![closure_slot.clone().into()],
                vec![
                    Closure {
                        function: Default::default(),
                        upvalues: vec![Upvalue::Ref(capture_param.clone())],
                    }
                    .into(),
                ],
            )
            .into(),
        );
        function.block_mut(merge).unwrap().extend([
            Assign::new(
                vec![written.clone().into()],
                vec![
                    Binary::new(
                        merge_param.clone().into(),
                        Literal::String(b"+mutated".to_vec().into()).into(),
                        BinaryOperation::Concat,
                    )
                    .into(),
                ],
            )
            .into(),
            Return::new(vec![written.clone().into()]).into(),
        ]);

        let mut to_captures = BlockEdge::new(BranchType::Then);
        to_captures
            .arguments
            .push((capture_param.clone(), entry_value.clone().into()));
        let mut bypass = BlockEdge::new(BranchType::Else);
        bypass
            .arguments
            .push((merge_param.clone(), entry_value.clone().into()));
        let mut to_merge = BlockEdge::default();
        to_merge
            .arguments
            .push((merge_param.clone(), capture_param.clone().into()));
        function.graph_mut().add_edge(entry, captures, to_captures);
        function.graph_mut().add_edge(entry, merge, bypass);
        function.graph_mut().add_edge(captures, merge, to_merge);

        // What `mark_upvalues` records: every value that aliases the box the
        // closure holds, which is the captured parameter, the value flowing
        // into the merge, and the write after it.
        let mut upvalue_to_group = IndexMap::from_iter([
            (capture_param, group.clone()),
            (merge_param, group.clone()),
            (written.clone(), group.clone()),
        ]);

        // Mirror the structuring pipeline: collapse redundant block parameters
        // until a fixed point, keeping the groups in step with every rename.
        loop {
            let mut local_map = FxHashMap::default();
            if !remove_unnecessary_params(&mut function, &mut local_map) {
                break;
            }
            apply_local_map_to_upvalue_groups(&mut upvalue_to_group, &local_map);
            apply_local_map(&mut function, local_map);
        }

        let captured = function
            .blocks()
            .flat_map(|(_, block)| block.iter())
            .filter_map(|statement| statement.as_assign())
            .flat_map(|assign| &assign.right)
            .filter_map(|value| value.as_closure())
            .flat_map(|closure| &closure.upvalues)
            .find_map(|upvalue| match upvalue {
                Upvalue::Ref(local) => Some(local.clone()),
                Upvalue::Copy(_) => None,
            })
            .expect("the closure must still capture by reference");
        assert_eq!(
            captured, entry_value,
            "collapsing the parameters must rewrite the capture onto the entry \
             definition, otherwise this shape does not exercise the rename"
        );

        assert_eq!(
            upvalue_to_group.get(&captured),
            Some(&group),
            "the captured value must still be a group member after renaming"
        );
        assert_eq!(
            upvalue_to_group.get(&written),
            Some(&group),
            "the write after the merge must share the captured value's group"
        );
    }
}
