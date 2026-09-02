use std::collections::BTreeMap;

use by_address::ByAddress;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use parking_lot::Mutex;
use petgraph::{
    Direction,
    algo::dominators::{Dominators, simple_fast},
    prelude::{DiGraph, NodeIndex},
};
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;

use crate::{Assign, Block, LocalRw, RcLocal, Statement};

#[derive(Default)]
pub struct LocalDeclarer {
    block_to_node: FxHashMap<ByAddress<Arc<Mutex<Block>>>, NodeIndex>,
    graph: DiGraph<(Option<Arc<Mutex<Block>>>, usize), ()>,
    lexical_parent: FxHashMap<NodeIndex, NodeIndex>,
    local_usages: IndexMap<RcLocal, FxHashMap<NodeIndex, usize>>,
    intrinsic_scope_nodes: FxHashMap<RcLocal, FxHashSet<NodeIndex>>,
    intrinsic_sites: FxHashMap<RcLocal, Vec<(NodeIndex, usize)>>,
    declarations: FxHashMap<ByAddress<Arc<Mutex<Block>>>, BTreeMap<usize, IndexSet<RcLocal>>>,
}

impl LocalDeclarer {
    fn add_lexical_edge(&mut self, parent: NodeIndex, child: NodeIndex) {
        self.graph.add_edge(parent, child, ());
        self.lexical_parent.insert(child, parent);
    }

    fn record_intrinsic_scope(&mut self, local: RcLocal, scope_root: NodeIndex) {
        self.intrinsic_scope_nodes
            .entry(local)
            .or_default()
            .insert(scope_root);
    }

    fn record_intrinsic_site(&mut self, local: RcLocal, node: NodeIndex, stat_index: usize) {
        self.intrinsic_sites
            .entry(local)
            .or_default()
            .push((node, stat_index));
    }

    fn is_in_intrinsic_scope(&self, local: &RcLocal, node: NodeIndex) -> bool {
        let Some(scope_roots) = self.intrinsic_scope_nodes.get(local) else {
            return false;
        };

        let mut current = node;
        loop {
            if scope_roots.contains(&current) {
                return true;
            }
            let Some(&parent) = self.lexical_parent.get(&current) else {
                return false;
            };
            current = parent;
        }
    }

    fn is_in_intrinsic_site_scope(
        &self,
        local: &RcLocal,
        node: NodeIndex,
        stat_index: usize,
    ) -> bool {
        let Some(sites) = self.intrinsic_sites.get(local) else {
            return false;
        };

        for &(site_node, site_index) in sites {
            if node == site_node {
                if stat_index >= site_index {
                    return true;
                }
                continue;
            }

            // A child block's stored statement index is the index of the
            // structured statement that introduced it. This makes the class
            // declaration visible in later nested blocks without leaking it
            // into earlier siblings.
            let mut current = node;
            while let Some(&parent) = self.lexical_parent.get(&current) {
                if parent == site_node {
                    let child_index = self.graph.node_weight(current).unwrap().1;
                    if child_index > site_index {
                        return true;
                    }
                    break;
                }
                current = parent;
            }
        }
        false
    }

    fn record_usage(&mut self, local: RcLocal, node: NodeIndex, stat_index: usize) {
        self.local_usages
            .entry(local)
            .or_default()
            .entry(node)
            .and_modify(|first| *first = (*first).min(stat_index))
            .or_insert(stat_index);
    }

    fn visit(&mut self, block: Arc<Mutex<Block>>, stat_index: usize) -> NodeIndex {
        let node = self.graph.add_node((Some(block.clone()), stat_index));
        self.block_to_node.insert(block.clone().into(), node);
        for (stat_index, stat) in block.lock().iter().enumerate() {
            match stat {
                Statement::Class(class) => {
                    // The class statement declares its target. Captures of that
                    // target in methods resolve at this site, not through a
                    // synthetic assignment declaration.
                    self.record_intrinsic_site(class.target.clone(), node, stat_index);
                    for local in class.values_read() {
                        self.record_usage(local.clone(), node, stat_index);
                    }
                }
                Statement::GenericFor(for_) => {
                    // Iterator values are evaluated in the enclosing scope;
                    // result locals are declared by the loop header.
                    for local in for_.values_read() {
                        self.record_usage(local.clone(), node, stat_index);
                    }
                }
                Statement::NumericFor(for_) => {
                    // Bounds are evaluated in the enclosing scope; the counter
                    // is declared by the loop header.
                    for local in for_.values_read() {
                        self.record_usage(local.clone(), node, stat_index);
                    }
                }
                // A repeat condition shares the body's lexical scope, so recording it
                // against the parent would incorrectly hoist body locals.
                Statement::Repeat(_) => {}
                _ => {
                    for local in stat.values() {
                        self.record_usage(local.clone(), node, stat_index);
                    }
                }
            }

            match stat {
                Statement::If(r#if) => {
                    let if_node = self.graph.add_node((None, stat_index));
                    self.add_lexical_edge(node, if_node);
                    let then_node = self.visit(r#if.then_block.clone(), stat_index);
                    self.add_lexical_edge(if_node, then_node);
                    let else_node = self.visit(r#if.else_block.clone(), stat_index);
                    self.add_lexical_edge(if_node, else_node);
                }
                Statement::Do(r#do) => {
                    let child = self.visit(r#do.block.clone(), stat_index);
                    self.add_lexical_edge(node, child);
                }
                Statement::While(r#while) => {
                    let child = self.visit(r#while.block.clone(), stat_index);
                    self.add_lexical_edge(node, child);
                }
                Statement::Repeat(repeat) => {
                    let child = self.visit(r#repeat.block.clone(), stat_index);
                    self.add_lexical_edge(node, child);
                    let condition_index = repeat.block.lock().len();
                    for local in repeat.condition.values_read() {
                        self.record_usage(local.clone(), child, condition_index);
                    }
                }
                Statement::NumericFor(numeric_for) => {
                    let child = self.visit(r#numeric_for.block.clone(), stat_index);
                    self.add_lexical_edge(node, child);
                    self.record_intrinsic_scope(numeric_for.counter.clone(), child);
                }
                Statement::GenericFor(generic_for) => {
                    let child = self.visit(r#generic_for.block.clone(), stat_index);
                    self.add_lexical_edge(node, child);
                    for local in &generic_for.res_locals {
                        self.record_intrinsic_scope(local.clone(), child);
                    }
                }
                _ => {}
            }
        }
        node
    }

    pub fn declare_locals(
        mut self,
        root_block: Arc<Mutex<Block>>,
        locals_to_ignore: &FxHashSet<RcLocal>,
    ) {
        let root_node = self.visit(root_block, 0);
        let dominators = simple_fast(&self.graph, root_node);
        let dominator_depths = self.dominator_depths(&dominators);
        let local_usages = std::mem::take(&mut self.local_usages);
        for (local, usages) in local_usages {
            if locals_to_ignore.contains(&local) {
                continue;
            }

            // Intrinsic loop bindings are scoped to their body, and class targets
            // are intrinsic at one statement site. Keep ordinary uses outside
            // those regions so a reused local identity still receives a valid
            // declaration where it is needed.
            let usages = usages
                .into_iter()
                .filter(|(node, stat_index)| {
                    !self.is_in_intrinsic_scope(&local, *node)
                        && !self.is_in_intrinsic_site_scope(&local, *node, *stat_index)
                })
                .collect_vec();
            if usages.is_empty() {
                continue;
            }

            let (mut node, mut first_stat_index) = if usages.len() == 1 {
                usages.into_iter().next().unwrap()
            } else {
                let mut usage_iter = usages.iter();
                let first_usage = usage_iter.next().unwrap().0;
                let common_dominator = usage_iter.fold(first_usage, |common, (usage, _)| {
                    nearest_common_dominator(&dominators, &dominator_depths, common, *usage)
                });
                let mut first_stat_index = usages.iter().find_map(|(node, stat_index)| {
                    (*node == common_dominator).then_some(*stat_index)
                });
                for child in self
                    .graph
                    .neighbors_directed(common_dominator, Direction::Outgoing)
                {
                    if usages.iter().any(|(usage, _)| {
                        dominators
                            .dominators(*usage)
                            .is_some_and(|path| path.into_iter().any(|node| node == child))
                    }) {
                        let child_index = self.graph.node_weight(child).unwrap().1;
                        first_stat_index = Some(
                            first_stat_index.map_or(child_index, |first| first.min(child_index)),
                        );
                    }
                }
                (common_dominator, first_stat_index.unwrap())
            };
            while let (block, parent_stat_index) = self.graph.node_weight(node).unwrap()
                && block.is_none()
            {
                let parent = self
                    .graph
                    .neighbors_directed(node, Direction::Incoming)
                    .exactly_one()
                    .unwrap();
                (node, first_stat_index) = (parent, *parent_stat_index);
            }
            let block = self
                .graph
                .node_weight(node)
                .unwrap()
                .0
                .as_ref()
                .unwrap()
                .clone();
            self.declarations
                .entry(block.into())
                .or_default()
                .entry(first_stat_index)
                .or_default()
                .insert(local);
        }

        for (ByAddress(block), declarations) in self.declarations {
            let mut block = block.lock();
            for (stat_index, mut locals) in declarations.into_iter().rev() {
                if let Some(Statement::Assign(assign)) = block.get_mut(stat_index)
                    && assign
                        .left
                        .iter()
                        .all(|l| l.as_local().is_some_and(|l| locals.contains(l)))
                {
                    locals.retain(|l| {
                        !assign
                            .left
                            .iter()
                            .map(|l| l.as_local().unwrap())
                            .contains(l)
                    });
                    assign.prefix = true;
                }
                if !locals.is_empty() {
                    let mut declaration =
                        Assign::new(locals.into_iter().map(|l| l.into()).collect_vec(), vec![]);
                    declaration.prefix = true;
                    block.insert(stat_index, declaration.into());
                }
            }
        }
    }

    fn dominator_depths(&self, dominators: &Dominators<NodeIndex>) -> FxHashMap<NodeIndex, usize> {
        let mut depths = FxHashMap::default();
        for node in self.graph.node_indices() {
            let mut current = node;
            let mut path = Vec::new();
            while !depths.contains_key(&current) {
                let Some(parent) = dominators.immediate_dominator(current) else {
                    depths.insert(current, 0);
                    break;
                };
                path.push(current);
                current = parent;
            }
            let mut depth = *depths.get(&current).unwrap();
            for node in path.into_iter().rev() {
                depth += 1;
                depths.insert(node, depth);
            }
        }
        depths
    }
}

fn nearest_common_dominator(
    dominators: &Dominators<NodeIndex>,
    depths: &FxHashMap<NodeIndex, usize>,
    mut left: NodeIndex,
    mut right: NodeIndex,
) -> NodeIndex {
    while depths[&left] > depths[&right] {
        left = dominators.immediate_dominator(left).unwrap();
    }
    while depths[&right] > depths[&left] {
        right = dominators.immediate_dominator(right).unwrap();
    }
    while left != right {
        left = dominators.immediate_dominator(left).unwrap();
        right = dominators.immediate_dominator(right).unwrap();
    }
    left
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rustc_hash::FxHashSet;

    use super::LocalDeclarer;
    use crate::{
        Assign, Block, Class, Literal, Local, NumericFor, RValue, RcLocal, Repeat, Return,
        Statement,
    };
    use parking_lot::Mutex;
    use triomphe::Arc;

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn declarations(block: &Block) -> BTreeSet<RcLocal> {
        block
            .iter()
            .filter_map(Statement::as_assign)
            .filter(|assign| assign.prefix)
            .flat_map(|assign| {
                assign
                    .left
                    .iter()
                    .filter_map(|value| value.as_local())
                    .cloned()
            })
            .collect()
    }

    #[test]
    fn repeat_condition_uses_the_body_scope() {
        let incoming = local("incoming");
        let snapshot = local("snapshot");
        let repeat = Repeat::new(
            RValue::Local(snapshot.clone()),
            Block(vec![
                Assign::new(vec![snapshot.clone().into()], vec![incoming.clone().into()]).into(),
            ]),
        );
        let repeat_body = repeat.block.clone();
        let root = Arc::new(Mutex::new(Block(vec![repeat.into()])));

        LocalDeclarer::default().declare_locals(root.clone(), &FxHashSet::from_iter([incoming]));

        assert!(declarations(&root.lock()).is_empty());
        let body = repeat_body.lock();
        assert!(body[0].as_assign().unwrap().prefix);
        assert_eq!(declarations(&body), BTreeSet::from([snapshot]));
    }

    #[test]
    fn loop_carried_value_used_after_repeat_is_declared_before_it() {
        let incoming = local("incoming");
        let carried = local("carried");
        let repeat = Repeat::new(
            RValue::Local(carried.clone()),
            Block(vec![
                Assign::new(
                    vec![carried.clone().into()],
                    vec![Literal::Boolean(true).into()],
                )
                .into(),
            ]),
        );
        let root = Arc::new(Mutex::new(Block(vec![
            repeat.into(),
            Return::new(vec![carried.clone().into()]).into(),
        ])));

        LocalDeclarer::default().declare_locals(root.clone(), &FxHashSet::from_iter([incoming]));

        let root = root.lock();
        assert_eq!(declarations(&root), BTreeSet::from([carried]));
        assert!(root[0].as_assign().unwrap().prefix);
        assert!(root[0].as_assign().unwrap().right.is_empty());
    }

    #[test]
    fn loop_binding_does_not_suppress_reused_local_outside_body() {
        let reused = local("reused");
        let numeric_for = NumericFor::new(
            Literal::Integer(0).into(),
            Literal::Integer(1).into(),
            Literal::Integer(1).into(),
            reused.clone(),
            Block(vec![Return::new(vec![reused.clone().into()]).into()]),
        );
        let root = Arc::new(Mutex::new(Block(vec![
            numeric_for.into(),
            Assign::new(
                vec![reused.clone().into()],
                vec![Literal::Boolean(true).into()],
            )
            .into(),
        ])));

        LocalDeclarer::default().declare_locals(root.clone(), &FxHashSet::default());

        let root = root.lock();
        assert!(root[0].as_numeric_for().is_some());
        assert!(root[1].as_assign().unwrap().prefix);
        let body = root[0].as_numeric_for().unwrap().block.lock();
        assert!(declarations(&body).is_empty());
    }

    #[test]
    fn class_binding_remains_visible_after_intrinsic_site() {
        let target = local("Target");
        let class = Class::new(target.clone(), "Target".to_owned(), Vec::new());
        let root = Arc::new(Mutex::new(Block(vec![
            class.into(),
            Assign::new(vec![target.into()], vec![Literal::Boolean(true).into()]).into(),
        ])));

        LocalDeclarer::default().declare_locals(root.clone(), &FxHashSet::default());

        let root = root.lock();
        assert!(declarations(&root).is_empty());
        assert!(!root[1].as_assign().unwrap().prefix);
    }
}
