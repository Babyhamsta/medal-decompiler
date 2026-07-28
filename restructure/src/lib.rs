#![feature(let_chains)]

use ast::{LocalRw, Reduce};
use cfg::{block::BranchType, function::Function};
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};

use petgraph::{
    algo::dominators::{Dominators, simple_fast},
    stable_graph::{EdgeIndex, NodeIndex, StableDiGraph},
    visit::*,
};
use tuple::Map;

mod conditional;
mod jump;
mod r#loop;

// TODO: REFACTOR: move
pub fn post_dominators<N: Default, E: Default>(
    graph: &mut StableDiGraph<N, E>,
) -> Dominators<NodeIndex> {
    let exits = graph
        .node_identifiers()
        .filter(|&n| graph.neighbors(n).count() == 0)
        .collect_vec();
    let fake_exit = graph.add_node(Default::default());
    for exit in exits {
        graph.add_edge(exit, fake_exit, Default::default());
    }
    let res = simple_fast(Reversed(&*graph), fake_exit);
    assert!(graph.remove_node(fake_exit).is_some());
    res
}

struct GraphStructurer {
    pub function: Function,
    loop_headers: FxHashSet<NodeIndex>,
    recovery_region_headers: FxHashSet<NodeIndex>,
    reachable_terminal_returns: Vec<ast::Return>,
    label_to_node: FxHashMap<ast::Label, NodeIndex>,
}

impl GraphStructurer {
    fn find_loop_headers(&mut self) {
        self.loop_headers.clear();
        depth_first_search(
            self.function.graph(),
            Some(self.function.entry().unwrap()),
            |event| {
                if let DfsEvent::BackEdge(_, header) = event {
                    self.loop_headers.insert(header);
                }
            },
        );
    }
    fn new(function: Function, recovery: &cfg::recovery::RecoveryFacts) -> Self {
        let reachable_terminal_returns = collect_region_terminal_returns(&function, recovery);
        let mut this = Self {
            function,
            loop_headers: FxHashSet::default(),
            recovery_region_headers: recovery
                .candidate_regions
                .iter()
                .map(|region| region.header)
                .collect(),
            reachable_terminal_returns,
            label_to_node: FxHashMap::default(),
        };
        this.find_loop_headers();
        this
    }

    fn block_is_no_op(block: &ast::Block) -> bool {
        !block.iter().any(|s| s.as_comment().is_none())
    }

    fn try_match_pattern(
        &mut self,
        node: NodeIndex,
        dominators: &Dominators<NodeIndex>,
        post_dom: &Dominators<NodeIndex>,
    ) -> bool {
        let successors = self.function.successor_blocks(node).collect_vec();

        // cfg::dot::render_to(&self.function, &mut std::io::stdout()).unwrap();
        if self.try_collapse_loop(node, dominators, post_dom) {
            self.find_loop_headers();
            // println!("matched loop");
            return true;
        }

        if self.try_remove_unnecessary_condition(node) {
            return true;
        }

        let changed = match successors.len() {
            0 => false,
            1 => {
                // remove unnecessary jumps to allow pattern matching
                self.match_jump(node, Some(successors[0]))
            }
            2 => {
                let (then_target, else_target) = self
                    .function
                    .conditional_edges(node)
                    .unwrap()
                    .map(|e| e.target());
                self.match_conditional(node, then_target, else_target)
            }

            _ => unreachable!(),
        };

        //println!("after");
        //dot::render_to(&self.function, &mut std::io::stdout()).unwrap();

        changed
    }

    fn match_blocks(&mut self) -> bool {
        let dfs = Dfs::new(self.function.graph(), self.function.entry().unwrap())
            .iter(self.function.graph())
            .collect::<FxHashSet<_>>();
        let mut dfs_postorder =
            DfsPostOrder::new(self.function.graph(), self.function.entry().unwrap());
        let mut dominators = simple_fast(self.function.graph(), self.function.entry().unwrap());
        let mut post_dom = post_dominators(self.function.graph_mut());

        // cfg::dot::render_to(&self.function, &mut std::io::stdout()).unwrap();

        let mut changed = false;
        while let Some(node) = dfs_postorder.next(self.function.graph()) {
            // println!("matching {:?}", node);
            let matched = self.try_match_pattern(node, &dominators, &post_dom);
            if matched {
                dominators = simple_fast(self.function.graph(), self.function.entry().unwrap());
                post_dom = post_dominators(self.function.graph_mut());
            }
            changed |= matched;
            // if matched {
            //     cfg::dot::render_to(&self.function, &mut std::io::stdout()).unwrap();
            // }
        }

        for node in self
            .function
            .graph()
            .node_indices()
            .filter(|node| !dfs.contains(node))
            .collect_vec()
        {
            // block may have been removed in a previous iteration
            if self.function.has_block(node)
                && self.function.predecessor_blocks(node).next().is_none()
            {
                if self
                    .function
                    .block(node)
                    .unwrap()
                    .first()
                    .and_then(|s| s.as_label())
                    .is_none()
                {
                    self.function.remove_block(node);
                } else {
                    //let dominators = simple_fast(self.function.graph(), node);
                    let matched = self.try_match_pattern(node, &dominators, &post_dom);
                    changed |= matched;
                }
            }
        }

        changed
    }

    fn insert_goto_for_edge(&mut self, edge: EdgeIndex) {
        let (source, target) = self.function.graph().edge_endpoints(edge).unwrap();
        if self.function.graph().edge_weight(edge).unwrap().branch_type == BranchType::Unconditional
            && self.function.predecessor_blocks(target).count() == 1
        {
            assert!(self.function.successor_blocks(source).count() == 1);
            // TODO: this code is repeated in match_jump, move to a new function
            let edges = self.function.remove_edges(target);
            let block = self.function.remove_block(target).unwrap();
            self.function.block_mut(source).unwrap().extend(block.0);
            self.function.set_edges(source, edges);
        } else {
            // TODO: make label an Rc and have a global counter for block name
            let label = ast::Label(format!("l{}", target.index()));
            let target_block = self.function.block_mut(target).unwrap();
            if target_block.first().and_then(|s| s.as_label()).is_none() {
                self.label_to_node.insert(label.clone(), target);
                target_block.insert(0, label.clone().into());
            }
            let goto_block = self.function.new_block();
            self.function
                .block_mut(goto_block)
                .unwrap()
                .push(ast::Goto::new(label).into());

            let edge = self.function.graph_mut().remove_edge(edge).unwrap();
            self.function.graph_mut().add_edge(source, goto_block, edge);
        }
    }

    fn split_edge_target(&mut self, edge: EdgeIndex) -> bool {
        let Some((source, target)) = self.function.graph().edge_endpoints(edge) else {
            return false;
        };
        if self.is_loop_header(target) || source == target {
            return false;
        }

        let target_block = self.function.block(target).unwrap().clone();
        let outgoing = self
            .function
            .graph()
            .edges(target)
            .map(|edge| (edge.target(), edge.weight().clone()))
            .collect::<Vec<_>>();
        let incoming = self.function.graph_mut().remove_edge(edge).unwrap();
        let duplicate = self.function.graph_mut().add_node(target_block);
        self.function
            .graph_mut()
            .add_edge(source, duplicate, incoming);
        for (successor, edge) in outgoing {
            self.function
                .graph_mut()
                .add_edge(duplicate, successor, edge);
        }
        true
    }

    fn remove_last_return(block: ast::Block) -> ast::Block {
        if let Some(ast::Statement::Return(last_statement)) = block.last() {
            if last_statement.values.is_empty() {
                let take = block.len() - 1;
                return block.0.into_iter().take(take).collect_vec().into();
            }
        }
        block
    }

    fn collapse(&mut self) {
        loop {
            while self.match_blocks() {}
            if self.function.graph().node_count() == 1 {
                break;
            }
            // last resort refinement
            let edges = self.function.graph().edge_indices().collect::<Vec<_>>();
            // https://edmcman.github.io/papers/usenix13.pdf
            // we prefer to remove edges whose source does not dominate its target, nor whose target dominates its source
            // TODO: try all possible paths and return the one with the least gotos, i don't think there's any other way
            // to get best output
            let mut changed = false;
            for &edge in &edges {
                // edge might have been invalidated by a previous iteration due to insert_goto_for_edge
                // calling remove_block(target)
                if self.function.graph().edge_weight(edge).is_none() {
                    continue;
                }

                let (source, target) = self.function.graph().edge_endpoints(edge).unwrap();
                let dominators = simple_fast(self.function.graph(), self.function.entry().unwrap());
                let target_dominators = dominators.dominators(target);
                let source_dominators = dominators.dominators(source);
                // TODO: check if blocks in dfs instead
                if target_dominators.is_none() || source_dominators.is_none() {
                    continue;
                }
                let mut target_dominators = target_dominators.unwrap();
                let mut source_dominators = source_dominators.unwrap();
                if target_dominators.contains(&source) || source_dominators.contains(&target) {
                    continue;
                }

                if self.split_edge_target(edge) {
                    self.find_loop_headers();
                    changed = self.match_blocks();
                } else {
                    self.insert_goto_for_edge(edge);
                    self.find_loop_headers();
                    changed = self.match_blocks();
                }
                if changed {
                    break;
                }
            }

            if !changed {
                for edge in edges {
                    // edge might have been invalidated by a previous iteration due to insert_goto_for_edge
                    // calling remove_block(target)
                    if self.function.graph().edge_weight(edge).is_none() {
                        continue;
                    }
                    self.insert_goto_for_edge(edge);
                    self.find_loop_headers();
                    changed = self.match_blocks();
                    if changed {
                        break;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
    }

    fn structure(mut self) -> ast::Block {
        self.collapse();
        let mut result = if self.function.graph().node_count() != 1 {
            let mut res_block = ast::Block::default();
            let entry = self.function.entry().unwrap();
            let mut stack = vec![entry];
            let mut visited = FxHashSet::default();
            while let Some(node) = stack.pop() {
                if visited.contains(&node) {
                    continue;
                }
                visited.insert(node);

                fn collect_gotos(block: &ast::Block, gotos: &mut FxHashSet<ast::Label>) {
                    for statement in &block.0 {
                        match statement {
                            ast::Statement::Goto(goto) => {
                                gotos.insert(goto.0.clone());
                            }
                            ast::Statement::If(r#if) => {
                                collect_gotos(&r#if.then_block.lock(), gotos);
                                collect_gotos(&r#if.else_block.lock(), gotos);
                            }
                            ast::Statement::While(r#while) => {
                                collect_gotos(&r#while.block.lock(), gotos);
                            }
                            ast::Statement::Repeat(repeat) => {
                                collect_gotos(&repeat.block.lock(), gotos);
                            }
                            ast::Statement::NumericFor(numeric_for) => {
                                collect_gotos(&numeric_for.block.lock(), gotos);
                            }
                            ast::Statement::GenericFor(generic_for) => {
                                collect_gotos(&generic_for.block.lock(), gotos);
                            }
                            _ => {}
                        }
                    }
                }

                let block = self.function.remove_block(node).unwrap();
                let mut goto_destinations = FxHashSet::default();
                collect_gotos(&block, &mut goto_destinations);
                for label in goto_destinations {
                    // TODO: block might have been merged/structured into another, output that block instead
                    // will require collecting label definitions in addition to references (gotos)
                    let target_node = self.label_to_node[&label];
                    if self.function.has_block(target_node) {
                        stack.push(target_node);
                    }
                }
                if let Some(ast::Statement::Goto(goto)) = res_block.last()
                // TODO: keep label -> block map instead
                    && goto.0.0[1..] == node.index().to_string()
                {
                    res_block.pop();
                }
                if !block
                    .first()
                    .is_some_and(|s| matches!(s, ast::Statement::Label(_)))
                {
                    res_block.push(ast::Comment::new(format!("block {}", node.index())).into());
                }
                res_block.extend(block.0)
            }
            // TODO: these nodes are never executed (i think), comment them out or dont include them
            for node in self.function.graph().node_indices().collect::<Vec<_>>() {
                let block = self.function.remove_block(node).unwrap();
                if !block
                    .first()
                    .is_some_and(|s| matches!(s, ast::Statement::Label(_)))
                {
                    res_block.push(ast::Comment::new(format!("block {}", node.index())).into());
                }
                res_block.extend(block.0)
            }

            res_block
        } else {
            Self::remove_last_return(
                self.function
                    .remove_block(self.function.entry().unwrap())
                    .unwrap(),
            )
        };
        flatten_single_iteration_loops(&mut result);
        relocate_unreachable_terminal_returns(&mut result, &self.reachable_terminal_returns);
        result
    }
}

fn flatten_single_iteration_loops(block: &mut ast::Block) -> usize {
    let mut changed = 0;
    for statement in &mut block.0 {
        changed += match statement {
            ast::Statement::If(if_) => {
                flatten_single_iteration_loops(&mut if_.then_block.lock())
                    + flatten_single_iteration_loops(&mut if_.else_block.lock())
            }
            ast::Statement::While(while_) => {
                flatten_single_iteration_loops(&mut while_.block.lock())
            }
            ast::Statement::Repeat(repeat) => {
                flatten_single_iteration_loops(&mut repeat.block.lock())
            }
            ast::Statement::NumericFor(for_) => {
                flatten_single_iteration_loops(&mut for_.block.lock())
            }
            ast::Statement::GenericFor(for_) => {
                flatten_single_iteration_loops(&mut for_.block.lock())
            }
            _ => 0,
        };
    }

    let mut index = 0;
    while index < block.len() {
        let Some(replacement) = block[index]
            .as_while()
            .and_then(single_iteration_loop_replacement)
        else {
            index += 1;
            continue;
        };
        block.0.splice(index..=index, replacement);
        changed += 1;
    }
    changed
}

fn single_iteration_loop_replacement(while_: &ast::While) -> Option<Vec<ast::Statement>> {
    if while_.condition != ast::RValue::Literal(ast::Literal::Boolean(true)) {
        return None;
    }
    let body = while_.block.lock();
    let (guard_index, execute_suffix_condition) =
        body.iter().enumerate().find_map(|(index, statement)| {
            let if_ = statement.as_if()?;
            let then_break = matches!(if_.then_block.lock().as_slice(), [ast::Statement::Break(_)]);
            let else_break = matches!(if_.else_block.lock().as_slice(), [ast::Statement::Break(_)]);
            if then_break && if_.else_block.lock().is_empty() {
                Some((
                    index,
                    ast::Unary::new(if_.condition.clone(), ast::UnaryOperation::Not)
                        .reduce_condition(),
                ))
            } else if else_break && if_.then_block.lock().is_empty() {
                Some((index, if_.condition.clone()))
            } else {
                None
            }
        })?;

    let mut suffix: ast::Block = body[guard_index + 1..].to_vec().into();
    if !strip_guaranteed_loop_exit(&mut suffix) {
        return None;
    }

    let mut replacement = body[..guard_index].to_vec();
    if !suffix.is_empty() {
        replacement
            .push(ast::If::new(execute_suffix_condition, suffix, ast::Block::default()).into());
    }
    Some(replacement)
}

fn strip_guaranteed_loop_exit(block: &mut ast::Block) -> bool {
    if matches!(block.last(), Some(ast::Statement::Break(_))) {
        block.pop();
        return true;
    }
    let Some(if_) = block.last().and_then(ast::Statement::as_if) else {
        return false;
    };
    if !if_.else_block.lock().is_empty()
        || !matches!(if_.then_block.lock().as_slice(), [ast::Statement::Break(_)])
        || !condition_proven_true(&if_.condition, &block[..block.len() - 1])
    {
        return false;
    }
    block.pop();
    true
}

fn condition_proven_true(condition: &ast::RValue, statements: &[ast::Statement]) -> bool {
    let ast::RValue::Binary(binary) = condition else {
        return false;
    };
    if binary.operation != ast::BinaryOperation::Equal {
        return false;
    }
    let (local, literal) = match (binary.left.as_ref(), binary.right.as_ref()) {
        (ast::RValue::Local(local), ast::RValue::Literal(literal))
        | (ast::RValue::Literal(literal), ast::RValue::Local(local)) => (local, literal),
        _ => return false,
    };
    statements.iter().rev().find_map(|statement| {
        if statement.values_written().contains(&local) {
            statement
                .as_assign()
                .filter(|assign| assign.left.len() == 1 && assign.right.len() == 1)
                .and_then(|assign| {
                    (assign.left[0].as_local() == Some(local)
                        && assign.right[0].as_literal() == Some(literal))
                    .then_some(true)
                })
        } else {
            None
        }
    }) == Some(true)
}

fn collect_region_terminal_returns(
    function: &Function,
    recovery: &cfg::recovery::RecoveryFacts,
) -> Vec<ast::Return> {
    let mut returns = Vec::new();
    for region in &recovery.candidate_regions {
        for mut target in recovery
            .edges
            .iter()
            .filter(|edge| {
                region.members.contains(&edge.source) && !region.members.contains(&edge.target)
            })
            .map(|edge| edge.target)
        {
            let mut visited = FxHashSet::default();
            while function.has_block(target) && visited.insert(target) {
                let block = function.block(target).unwrap();
                if let Some(return_) = block.iter().find_map(ast::Statement::as_return) {
                    if !returns.contains(return_) {
                        returns.push(return_.clone());
                    }
                    break;
                }
                if block.iter().any(|statement| {
                    statement.as_comment().is_none() && statement.as_empty().is_none()
                }) {
                    break;
                }
                let Some(next) = function.successor_blocks(target).exactly_one().ok() else {
                    break;
                };
                target = next;
            }
        }
    }
    returns
}

fn relocate_unreachable_terminal_returns(block: &mut ast::Block, templates: &[ast::Return]) {
    for template in templates {
        let mut removed = 0;
        remove_unreachable_return_copies(block, template, true, &mut removed);
        if removed > 0 && !contains_reachable_return(block, template, true) {
            block.push(template.clone().into());
        }
    }
}

fn remove_unreachable_return_copies(
    block: &mut ast::Block,
    template: &ast::Return,
    reachable: bool,
    removed: &mut usize,
) {
    block.retain(|statement| {
        if !reachable && statement.as_return() == Some(template) {
            *removed += 1;
            false
        } else {
            true
        }
    });
    for statement in &mut block.0 {
        match statement {
            ast::Statement::If(if_) => {
                let (then_reachable, else_reachable) = match if_.condition {
                    ast::RValue::Literal(ast::Literal::Boolean(value)) => {
                        (reachable && value, reachable && !value)
                    }
                    _ => (reachable, reachable),
                };
                remove_unreachable_return_copies(
                    &mut if_.then_block.lock(),
                    template,
                    then_reachable,
                    removed,
                );
                remove_unreachable_return_copies(
                    &mut if_.else_block.lock(),
                    template,
                    else_reachable,
                    removed,
                );
            }
            ast::Statement::While(while_) => remove_unreachable_return_copies(
                &mut while_.block.lock(),
                template,
                reachable,
                removed,
            ),
            ast::Statement::Repeat(repeat) => remove_unreachable_return_copies(
                &mut repeat.block.lock(),
                template,
                reachable,
                removed,
            ),
            ast::Statement::NumericFor(for_) => remove_unreachable_return_copies(
                &mut for_.block.lock(),
                template,
                reachable,
                removed,
            ),
            ast::Statement::GenericFor(for_) => remove_unreachable_return_copies(
                &mut for_.block.lock(),
                template,
                reachable,
                removed,
            ),
            _ => {}
        }
    }
}

fn contains_reachable_return(block: &ast::Block, template: &ast::Return, reachable: bool) -> bool {
    block.iter().any(|statement| {
        if reachable && statement.as_return() == Some(template) {
            return true;
        }
        match statement {
            ast::Statement::If(if_) => {
                let (then_reachable, else_reachable) = match if_.condition {
                    ast::RValue::Literal(ast::Literal::Boolean(value)) => {
                        (reachable && value, reachable && !value)
                    }
                    _ => (reachable, reachable),
                };
                contains_reachable_return(&if_.then_block.lock(), template, then_reachable)
                    || contains_reachable_return(&if_.else_block.lock(), template, else_reachable)
            }
            ast::Statement::While(while_) => {
                contains_reachable_return(&while_.block.lock(), template, reachable)
            }
            ast::Statement::Repeat(repeat) => {
                contains_reachable_return(&repeat.block.lock(), template, reachable)
            }
            ast::Statement::NumericFor(for_) => {
                contains_reachable_return(&for_.block.lock(), template, reachable)
            }
            ast::Statement::GenericFor(for_) => {
                contains_reachable_return(&for_.block.lock(), template, reachable)
            }
            _ => false,
        }
    })
}

pub fn lift(
    function: cfg::function::Function,
    recovery: &cfg::recovery::RecoveryFacts,
) -> ast::Block {
    GraphStructurer::new(function, recovery).structure()
}

#[cfg(test)]
mod tests {
    use crate::{
        contains_reachable_return, flatten_single_iteration_loops, lift,
        relocate_unreachable_terminal_returns,
    };
    use ast::{
        Assign, Binary, BinaryOperation, If, LValue, Literal, Local, RValue, RcLocal, Return,
        Statement,
    };
    use cfg::{
        block::{BlockEdge, BranchType},
        function::Function,
        provenance::BindingIdentity,
        recovery::RecoveryFacts,
    };

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn assign(target: &RcLocal, value: RValue) -> Statement {
        Assign::new(vec![LValue::Local(target.clone())], vec![value]).into()
    }

    #[test]
    fn loop_carried_latch_recovers_repeat_and_terminal_value() {
        let mut function = Function::new(0);
        let entry = function.new_block();
        let header = function.new_block();
        let latch = function.new_block();
        let exit = function.new_block();
        function.set_entry(entry);

        let previous = local("previous");
        let next = local("next");
        let count = local("count");
        for (register, value) in [&previous, &next, &count].into_iter().enumerate() {
            function.set_binding(value.clone(), BindingIdentity::local(0, register));
        }

        function
            .block_mut(entry)
            .unwrap()
            .push(assign(&count, Literal::Integer(0).into()));
        function.block_mut(header).unwrap().extend([
            assign(
                &next,
                Binary::new(
                    previous.clone().into(),
                    Literal::Integer(1).into(),
                    BinaryOperation::Add,
                )
                .into(),
            ),
            assign(
                &count,
                Binary::new(
                    count.clone().into(),
                    Literal::Integer(1).into(),
                    BinaryOperation::Add,
                )
                .into(),
            ),
            If::new(
                Binary::new(
                    next.clone().into(),
                    previous.clone().into(),
                    BinaryOperation::Equal,
                )
                .into(),
                Default::default(),
                Default::default(),
            )
            .into(),
        ]);
        function
            .block_mut(latch)
            .unwrap()
            .push(assign(&previous, next.clone().into()));
        function
            .block_mut(exit)
            .unwrap()
            .push(Return::new(vec![next.clone().into(), count.into()]).into());

        function
            .graph_mut()
            .add_edge(entry, header, BlockEdge::new(BranchType::Unconditional));
        function
            .graph_mut()
            .add_edge(header, exit, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(header, latch, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(latch, header, BlockEdge::new(BranchType::Unconditional));

        let facts = RecoveryFacts::derive(&function).unwrap();
        let block = lift(function, &facts);

        assert_eq!(
            block
                .iter()
                .filter(|statement| matches!(statement, Statement::Repeat(_)))
                .count(),
            1
        );
        assert_eq!(
            block
                .iter()
                .filter(|statement| matches!(statement, Statement::While(_)))
                .count(),
            0
        );
        let returned = block.last().unwrap().as_return().unwrap();
        assert_eq!(returned.values[0], previous.clone().into());
        assert!(facts.candidate_regions[0].members.contains(&header));
    }

    #[test]
    fn reachable_region_return_is_moved_out_of_constant_false_scaffold() {
        let value = local("value");
        let terminal = Return::new(vec![value.into()]);
        let mut block = ast::Block(vec![
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![
                    If::new(
                        Literal::Boolean(false).into(),
                        ast::Block(vec![terminal.clone().into()]),
                        Default::default(),
                    )
                    .into(),
                    ast::Break {}.into(),
                ]),
            )
            .into(),
        ]);

        relocate_unreachable_terminal_returns(&mut block, std::slice::from_ref(&terminal));

        assert!(contains_reachable_return(&block, &terminal, true));
        assert_eq!(block.last().unwrap().as_return(), Some(&terminal));
        let outer = block.first().unwrap().as_while().unwrap();
        assert!(!contains_reachable_return(
            &outer.block.lock(),
            &terminal,
            true
        ));
    }

    #[test]
    fn proven_single_iteration_loop_becomes_guarded_straight_line_region() {
        let event = local("event");
        let state = local("state");
        let mut block = ast::Block(vec![
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![
                    assign(&event, Literal::String(b"tick".to_vec()).into()),
                    If::new(
                        Binary::new(
                            event.clone().into(),
                            Literal::Nil.into(),
                            BinaryOperation::NotEqual,
                        )
                        .into(),
                        ast::Block(vec![ast::Break {}.into()]),
                        Default::default(),
                    )
                    .into(),
                    assign(&state, Literal::String(b"done".to_vec()).into()),
                    If::new(
                        Binary::new(
                            state.clone().into(),
                            Literal::String(b"done".to_vec()).into(),
                            BinaryOperation::Equal,
                        )
                        .into(),
                        ast::Block(vec![ast::Break {}.into()]),
                        Default::default(),
                    )
                    .into(),
                ]),
            )
            .into(),
            Return::new(vec![event.into(), state.into()]).into(),
        ]);

        assert_eq!(flatten_single_iteration_loops(&mut block), 1);
        assert!(block.iter().all(|statement| statement.as_while().is_none()));
        assert_eq!(block.len(), 3);
        assert!(block[1].as_if().is_some());
    }
}
