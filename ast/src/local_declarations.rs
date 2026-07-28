use std::collections::BTreeMap;

use array_tool::vec::Intersect;
use by_address::ByAddress;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use parking_lot::Mutex;
use petgraph::{
    Direction,
    algo::dominators::simple_fast,
    prelude::{DiGraph, NodeIndex},
};
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;

use crate::{Assign, Block, LocalRw, RcLocal, Statement};

#[derive(Default)]
pub struct LocalDeclarer {
    block_to_node: FxHashMap<ByAddress<Arc<Mutex<Block>>>, NodeIndex>,
    graph: DiGraph<(Option<Arc<Mutex<Block>>>, usize), ()>,
    local_usages: IndexMap<RcLocal, FxHashMap<NodeIndex, usize>>,
    intrinsic_declarations: FxHashSet<RcLocal>,
    declarations: FxHashMap<ByAddress<Arc<Mutex<Block>>>, BTreeMap<usize, IndexSet<RcLocal>>>,
}

impl LocalDeclarer {
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
                    self.intrinsic_declarations.insert(class.target.clone());
                }
                Statement::GenericFor(for_) => {
                    self.intrinsic_declarations
                        .extend(for_.res_locals.iter().cloned());
                }
                Statement::NumericFor(for_) => {
                    self.intrinsic_declarations.insert(for_.counter.clone());
                }
                _ => {}
            }

            // A repeat condition shares the body's lexical scope, so recording it
            // against the parent would incorrectly hoist body locals.
            if !matches!(stat, Statement::Repeat(_)) {
                for local in stat.values() {
                    self.record_usage(local.clone(), node, stat_index);
                }
            }

            match stat {
                Statement::If(r#if) => {
                    let if_node = self.graph.add_node((None, stat_index));
                    self.graph.add_edge(node, if_node, ());
                    let then_node = self.visit(r#if.then_block.clone(), stat_index);
                    self.graph.add_edge(if_node, then_node, ());
                    let else_node = self.visit(r#if.else_block.clone(), stat_index);
                    self.graph.add_edge(if_node, else_node, ());
                }
                Statement::While(r#while) => {
                    let child = self.visit(r#while.block.clone(), stat_index);
                    self.graph.add_edge(node, child, ());
                }
                Statement::Repeat(repeat) => {
                    let child = self.visit(r#repeat.block.clone(), stat_index);
                    self.graph.add_edge(node, child, ());
                    let condition_index = repeat.block.lock().len();
                    for local in repeat.condition.values_read() {
                        self.record_usage(local.clone(), child, condition_index);
                    }
                }
                Statement::NumericFor(numeric_for) => {
                    let child = self.visit(r#numeric_for.block.clone(), stat_index);
                    self.graph.add_edge(node, child, ());
                }
                Statement::GenericFor(generic_for) => {
                    let child = self.visit(r#generic_for.block.clone(), stat_index);
                    self.graph.add_edge(node, child, ());
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
        for (local, usages) in self.local_usages {
            if locals_to_ignore.contains(&local) || self.intrinsic_declarations.contains(&local) {
                continue;
            }
            let (mut node, mut first_stat_index) = if usages.len() == 1 {
                usages.into_iter().next().unwrap()
            } else {
                let node_dominators = usages
                    .keys()
                    .map(|&n| dominators.dominators(n).unwrap().collect_vec())
                    .collect_vec();
                let mut dom_iter = node_dominators.iter().cloned();
                let mut common_dominators = dom_iter.next().unwrap();
                for node_dominators in dom_iter {
                    common_dominators = common_dominators.intersect(node_dominators);
                }
                let common_dominator = common_dominators[0];
                let mut first_stat_index = usages.get(&common_dominator).copied();
                for child in self
                    .graph
                    .neighbors_directed(common_dominator, Direction::Outgoing)
                {
                    if node_dominators
                        .iter()
                        .any(|usage_dominators| usage_dominators.contains(&child))
                    {
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
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rustc_hash::FxHashSet;

    use super::LocalDeclarer;
    use crate::{Assign, Block, Literal, Local, RValue, RcLocal, Repeat, Return, Statement};
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
}
