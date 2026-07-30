#![feature(let_chains)]

use ast::{LocalRw, Reduce, Traverse};
use cfg::{block::BranchType, function::Function};
use itertools::Itertools;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;

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
                .candidate_regions()
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
        // Loop exits first: an unrecovered `goto` in the interior disqualifies
        // the whole block from terminal back-edge recovery below.
        if recover_loop_exit_breaks(&mut result) {
            let mut referenced = FxHashSet::default();
            collect_referenced_labels(&result, &mut referenced);
            remove_unreferenced_labels(&mut result, &referenced);
        }
        recover_terminal_backedge_loop(&mut result);
        flatten_single_iteration_loops(&mut result);
        relocate_unreachable_terminal_returns(&mut result, &self.reachable_terminal_returns);
        result
    }
}

/// The body of a loop statement, if this statement is a loop.
fn loop_body(statement: &ast::Statement) -> Option<&Arc<Mutex<ast::Block>>> {
    match statement {
        ast::Statement::While(r#while) => Some(&r#while.block),
        ast::Statement::Repeat(repeat) => Some(&repeat.block),
        ast::Statement::NumericFor(numeric_for) => Some(&numeric_for.block),
        ast::Statement::GenericFor(generic_for) => Some(&generic_for.block),
        _ => None,
    }
}

/// Rewrites `goto L` as `break` inside one loop level.
///
/// Nested loops are not descended into: `break` binds to the innermost
/// enclosing loop, so a jump from inside a nested loop to the outer loop's
/// exit is not expressible as a plain `break` and is left alone. Closures are
/// not descended into either, since they are separate functions.
fn replace_exit_gotos_with_break(block: &mut ast::Block, label: &ast::Label) -> bool {
    let mut changed = false;
    for statement in &mut block.0 {
        match statement {
            ast::Statement::Goto(goto) if &goto.0 == label => {
                *statement = ast::Break {}.into();
                changed = true;
            }
            ast::Statement::If(r#if) => {
                changed |= replace_exit_gotos_with_break(&mut r#if.then_block.lock(), label);
                changed |= replace_exit_gotos_with_break(&mut r#if.else_block.lock(), label);
            }
            _ => {}
        }
    }
    changed
}

/// Rewrites jumps to the statement immediately after a loop as `break`.
///
/// Restructuring emits `goto L`, with `::L::` placed directly after the
/// enclosing loop, when it cannot express that exit structurally. Jumping to
/// the point just past a loop is exactly what `break` means. Recovering it
/// matters beyond readability: an interior jump disqualifies the surrounding
/// block from [`recover_terminal_backedge_loop`], so one unrecovered exit can
/// leave an entire function unstructured.
fn recover_loop_exit_breaks(block: &mut ast::Block) -> bool {
    let mut changed = false;

    for statement in &mut block.0 {
        match statement {
            ast::Statement::If(r#if) => {
                changed |= recover_loop_exit_breaks(&mut r#if.then_block.lock());
                changed |= recover_loop_exit_breaks(&mut r#if.else_block.lock());
            }
            ast::Statement::While(r#while) => {
                changed |= recover_loop_exit_breaks(&mut r#while.block.lock());
            }
            ast::Statement::Repeat(repeat) => {
                changed |= recover_loop_exit_breaks(&mut repeat.block.lock());
            }
            ast::Statement::NumericFor(numeric_for) => {
                changed |= recover_loop_exit_breaks(&mut numeric_for.block.lock());
            }
            ast::Statement::GenericFor(generic_for) => {
                changed |= recover_loop_exit_breaks(&mut generic_for.block.lock());
            }
            _ => {}
        }
    }

    for index in 0..block.len() {
        let Some(label) = block.0.get(index + 1).and_then(ast::Statement::as_label).cloned() else {
            continue;
        };
        let Some(body) = loop_body(&block.0[index]).cloned() else {
            continue;
        };
        changed |= replace_exit_gotos_with_break(&mut body.lock(), &label);
    }

    changed
}

/// Collects every label still referenced by a `goto`.
fn collect_referenced_labels(block: &ast::Block, referenced: &mut FxHashSet<ast::Label>) {
    for statement in &block.0 {
        match statement {
            ast::Statement::Goto(goto) => {
                referenced.insert(goto.0.clone());
            }
            ast::Statement::If(r#if) => {
                collect_referenced_labels(&r#if.then_block.lock(), referenced);
                collect_referenced_labels(&r#if.else_block.lock(), referenced);
            }
            _ => {
                if let Some(body) = loop_body(statement) {
                    collect_referenced_labels(&body.lock(), referenced);
                }
            }
        }
    }
}

/// Drops label definitions that no `goto` targets any more.
fn remove_unreferenced_labels(block: &mut ast::Block, referenced: &FxHashSet<ast::Label>) -> bool {
    let mut changed = false;
    for statement in &mut block.0 {
        match statement {
            ast::Statement::If(r#if) => {
                changed |= remove_unreferenced_labels(&mut r#if.then_block.lock(), referenced);
                changed |= remove_unreferenced_labels(&mut r#if.else_block.lock(), referenced);
            }
            _ => {
                if let Some(body) = loop_body(statement) {
                    let body = body.clone();
                    let mut body = body.lock();
                    changed |= remove_unreferenced_labels(&mut body, referenced);
                }
            }
        }
    }

    let before = block.len();
    block.0.retain(|statement| match statement {
        ast::Statement::Label(label) => referenced.contains(label),
        _ => true,
    });
    changed || block.len() != before
}

fn recover_terminal_backedge_loop(block: &mut ast::Block) -> bool {
    let Some(label) = block.first().and_then(ast::Statement::as_label).cloned() else {
        return false;
    };
    let Some(goto) = block.last().and_then(ast::Statement::as_goto) else {
        return false;
    };
    if goto.0 != label || contains_unstructured_jump(&block[1..block.len() - 1]) {
        return false;
    }

    let mut statements = std::mem::take(&mut block.0);
    statements.pop();
    statements.remove(0);
    block.push(ast::While::new(ast::Literal::Boolean(true).into(), ast::Block(statements)).into());
    true
}

fn contains_unstructured_jump(statements: &[ast::Statement]) -> bool {
    statements.iter().any(|statement| match statement {
        ast::Statement::Goto(_) | ast::Statement::Label(_) => true,
        ast::Statement::If(if_) => {
            contains_unstructured_jump(&if_.then_block.lock())
                || contains_unstructured_jump(&if_.else_block.lock())
        }
        ast::Statement::While(while_) => contains_unstructured_jump(&while_.block.lock()),
        ast::Statement::Repeat(repeat) => contains_unstructured_jump(&repeat.block.lock()),
        ast::Statement::NumericFor(for_) => contains_unstructured_jump(&for_.block.lock()),
        ast::Statement::GenericFor(for_) => contains_unstructured_jump(&for_.block.lock()),
        _ => false,
    })
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

    if contains_outer_loop_transfer(&body[..guard_index]) {
        return None;
    }

    let mut suffix: ast::Block = body[guard_index + 1..].to_vec().into();
    if !strip_guaranteed_loop_exit(&mut suffix) {
        return None;
    }
    if contains_outer_loop_transfer(&suffix) {
        return None;
    }

    let mut replacement = body[..guard_index].to_vec();
    if !suffix.is_empty() {
        replacement
            .push(ast::If::new(execute_suffix_condition, suffix, ast::Block::default()).into());
    }
    Some(replacement)
}

fn contains_outer_loop_transfer(statements: &[ast::Statement]) -> bool {
    statements.iter().any(|statement| match statement {
        ast::Statement::Break(_) | ast::Statement::Continue(_) => true,
        ast::Statement::If(if_) => {
            contains_outer_loop_transfer(&if_.then_block.lock())
                || contains_outer_loop_transfer(&if_.else_block.lock())
        }
        // Break and continue inside a nested loop target that nested loop.
        ast::Statement::While(_)
        | ast::Statement::Repeat(_)
        | ast::Statement::NumericFor(_)
        | ast::Statement::GenericFor(_) => false,
        _ => false,
    })
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
    for statement in statements.iter().rev() {
        if statement.values_written().contains(&local) {
            return statement.as_assign().is_some_and(|assign| {
                assign.left.len() == 1
                    && assign.right.len() == 1
                    && assign.left[0].as_local() == Some(local)
                    && assign.right[0].as_literal() == Some(literal)
            });
        }
        if statement_may_change_local(statement, local) || statement_may_invoke_callback(statement)
        {
            return false;
        }
    }
    false
}

fn statement_may_change_local(statement: &ast::Statement, local: &ast::RcLocal) -> bool {
    if statement.values_written().contains(&local) {
        return true;
    }
    match statement {
        ast::Statement::If(if_) => {
            if_.then_block
                .lock()
                .iter()
                .any(|statement| statement_may_change_local(statement, local))
                || if_
                    .else_block
                    .lock()
                    .iter()
                    .any(|statement| statement_may_change_local(statement, local))
        }
        ast::Statement::While(while_) => while_
            .block
            .lock()
            .iter()
            .any(|statement| statement_may_change_local(statement, local)),
        ast::Statement::Repeat(repeat) => repeat
            .block
            .lock()
            .iter()
            .any(|statement| statement_may_change_local(statement, local)),
        ast::Statement::NumericFor(for_) => for_
            .block
            .lock()
            .iter()
            .any(|statement| statement_may_change_local(statement, local)),
        ast::Statement::GenericFor(for_) => for_
            .block
            .lock()
            .iter()
            .any(|statement| statement_may_change_local(statement, local)),
        _ => false,
    }
}

fn rvalue_may_invoke_callback(value: &ast::RValue) -> bool {
    matches!(
        value,
        ast::RValue::Call(_)
            | ast::RValue::MethodCall(_)
            | ast::RValue::Select(ast::Select::Call(_) | ast::Select::MethodCall(_))
    ) || value.rvalues().into_iter().any(rvalue_may_invoke_callback)
}

fn statement_may_invoke_callback(statement: &ast::Statement) -> bool {
    if matches!(
        statement,
        ast::Statement::Call(_) | ast::Statement::MethodCall(_)
    ) || statement
        .rvalues()
        .into_iter()
        .any(rvalue_may_invoke_callback)
    {
        return true;
    }
    match statement {
        ast::Statement::If(if_) => {
            if_.then_block
                .lock()
                .iter()
                .any(statement_may_invoke_callback)
                || if_
                    .else_block
                    .lock()
                    .iter()
                    .any(statement_may_invoke_callback)
        }
        ast::Statement::While(while_) => while_
            .block
            .lock()
            .iter()
            .any(statement_may_invoke_callback),
        ast::Statement::Repeat(repeat) => repeat
            .block
            .lock()
            .iter()
            .any(statement_may_invoke_callback),
        ast::Statement::NumericFor(for_) => {
            for_.block.lock().iter().any(statement_may_invoke_callback)
        }
        ast::Statement::GenericFor(for_) => {
            for_.block.lock().iter().any(statement_may_invoke_callback)
        }
        _ => false,
    }
}

fn collect_region_terminal_returns(
    function: &Function,
    recovery: &cfg::recovery::RecoveryFacts,
) -> Vec<ast::Return> {
    let mut returns = Vec::new();
    let edges = recovery
        .edges()
        .expect("reconstruction facts must carry edge facts");
    for region in recovery.candidate_regions() {
        for mut target in edges
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
        collect_referenced_labels, contains_reachable_return, flatten_single_iteration_loops, lift,
        recover_loop_exit_breaks, recover_terminal_backedge_loop, remove_unreferenced_labels,
        relocate_unreachable_terminal_returns,
    };
    use ast::{
        Assign, Binary, BinaryOperation, Call, Global, If, LValue, Literal, Local, RValue, RcLocal,
        Return, Statement,
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
    fn terminal_backedge_becomes_infinite_loop() {
        let label = ast::Label("loop".to_owned());
        let mut block = ast::Block(vec![
            label.clone().into(),
            ast::Comment::new("body".to_owned()).into(),
            ast::Goto::new(label).into(),
        ]);

        assert!(recover_terminal_backedge_loop(&mut block));
        assert_eq!(block.len(), 1);
        let loop_ = block[0].as_while().unwrap();
        assert_eq!(
            loop_.condition,
            ast::RValue::Literal(ast::Literal::Boolean(true))
        );
        assert_eq!(loop_.block.lock().len(), 1);
    }

    #[test]
    fn terminal_backedge_with_internal_jump_stays_explicit() {
        let label = ast::Label("loop".to_owned());
        let mut block = ast::Block(vec![
            label.clone().into(),
            ast::Goto::new(label.clone()).into(),
            ast::Goto::new(label).into(),
        ]);

        assert!(!recover_terminal_backedge_loop(&mut block));
        assert_eq!(block.len(), 3);
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
        assert!(facts.candidate_regions()[0].members.contains(&header));
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

    #[test]
    fn continue_in_prefix_prevents_single_iteration_flattening() {
        let mut block = ast::Block(vec![
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![
                    If::new(
                        Call::new(Global::from("retry").into(), Vec::new()).into(),
                        ast::Block(vec![ast::Continue {}.into()]),
                        Default::default(),
                    )
                    .into(),
                    If::new(
                        Call::new(Global::from("stop").into(), Vec::new()).into(),
                        ast::Block(vec![ast::Break {}.into()]),
                        Default::default(),
                    )
                    .into(),
                    ast::Break {}.into(),
                ]),
            )
            .into(),
        ]);

        assert_eq!(flatten_single_iteration_loops(&mut block), 0);
        assert!(block[0].as_while().is_some());
    }

    #[test]
    fn intervening_call_invalidates_literal_exit_proof() {
        let state = local("state");
        let mut block = ast::Block(vec![
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![
                    If::new(
                        Literal::Boolean(false).into(),
                        ast::Block(vec![ast::Break {}.into()]),
                        Default::default(),
                    )
                    .into(),
                    assign(&state, Literal::String(b"done".to_vec()).into()),
                    Call::new(Global::from("mutateCapturedState").into(), Vec::new()).into(),
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
        ]);

        assert_eq!(flatten_single_iteration_loops(&mut block), 0);
        assert!(block[0].as_while().is_some());
    }

    fn drop_unreferenced_labels(block: &mut ast::Block) {
        let mut referenced = rustc_hash::FxHashSet::default();
        collect_referenced_labels(block, &mut referenced);
        remove_unreferenced_labels(block, &referenced);
    }

    #[test]
    fn jump_to_statement_after_loop_becomes_break() {
        let exit = ast::Label("exit".to_owned());
        let mut block = ast::Block(vec![
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![ast::Goto::new(exit.clone()).into()]),
            )
            .into(),
            Statement::Label(exit),
        ]);

        assert!(recover_loop_exit_breaks(&mut block));
        drop_unreferenced_labels(&mut block);

        assert_eq!(block.len(), 1, "the label should be gone");
        let body = block[0].as_while().unwrap().block.lock();
        assert!(matches!(body[0], Statement::Break(_)));
    }

    #[test]
    fn jump_from_a_nested_loop_is_left_alone() {
        // `break` binds to the innermost loop, so this jump is not expressible
        // as a plain break and must survive untouched.
        let exit = ast::Label("exit".to_owned());
        let inner = ast::While::new(
            Literal::Boolean(true).into(),
            ast::Block(vec![ast::Goto::new(exit.clone()).into()]),
        );
        let mut block = ast::Block(vec![
            ast::While::new(Literal::Boolean(true).into(), ast::Block(vec![inner.into()])).into(),
            Statement::Label(exit),
        ]);

        assert!(!recover_loop_exit_breaks(&mut block));
        let outer = block[0].as_while().unwrap().block.lock();
        let inner = outer[0].as_while().unwrap().block.lock();
        assert!(matches!(inner[0], Statement::Goto(_)));
    }

    #[test]
    fn recovered_break_unblocks_terminal_backedge_recovery() {
        // The pattern that left a whole function unstructured: a back edge
        // spanning the body, with an unrecovered loop exit inside it.
        let top = ast::Label("l0".to_owned());
        let exit = ast::Label("l8".to_owned());
        let mut block = ast::Block(vec![
            Statement::Label(top.clone()),
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![ast::Goto::new(exit.clone()).into()]),
            )
            .into(),
            Statement::Label(exit),
            ast::Goto::new(top).into(),
        ]);

        assert!(
            !recover_terminal_backedge_loop(&mut block.clone()),
            "the interior jump should block recovery before the break is found"
        );

        assert!(recover_loop_exit_breaks(&mut block));
        drop_unreferenced_labels(&mut block);
        assert!(recover_terminal_backedge_loop(&mut block));

        assert_eq!(block.len(), 1);
        assert!(block[0].as_while().is_some());
    }
}
