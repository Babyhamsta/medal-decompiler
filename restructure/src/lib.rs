#![feature(let_chains)]

use ast::{LocalRw, Reduce, Traverse};
use cfg::{block::BranchType, function::Function};
use itertools::Itertools;
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
use triomphe::Arc;

use petgraph::{
    algo::dominators::{Dominators, simple_fast},
    stable_graph::{NodeIndex, StableDiGraph},
    visit::*,
};
use tuple::Map;

mod conditional;
mod jump;
mod r#loop;

/// `petgraph::simple_fast` is quadratic in the number of nodes. Structuring
/// computes dominators and post-dominators repeatedly after graph mutations,
/// so graphs above this single-pass work estimate use the bounded dispatcher
/// directly instead of cloning the graph and entering unbounded analysis.
const MAX_COLLAPSE_NODE_PAIRS: usize = 100_000_000;

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
}

impl GraphStructurer {
    fn collapse_within_budget(node_count: usize) -> bool {
        node_count.saturating_mul(node_count) <= MAX_COLLAPSE_NODE_PAIRS
    }

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

    fn remove_last_return(block: ast::Block) -> ast::Block {
        if let Some(ast::Statement::Return(last_statement)) = block.last() {
            if last_statement.values.is_empty() {
                let take = block.len() - 1;
                return block.0.into_iter().take(take).collect_vec().into();
            }
        }
        block
    }

    fn collapse(&mut self) -> bool {
        while self.match_blocks() {}
        self.function.graph().node_count() == 1
    }

    fn append_dispatch_transfer(
        block: &mut ast::Block,
        state: &ast::RcLocal,
        target: NodeIndex,
        edge: &cfg::block::BlockEdge,
    ) {
        if !edge.arguments.is_empty() {
            let mut arguments = ast::Assign::new(
                edge.arguments
                    .iter()
                    .map(|(parameter, _)| ast::LValue::Local(parameter.clone()))
                    .collect(),
                edge.arguments
                    .iter()
                    .map(|(_, argument)| argument.clone())
                    .collect(),
            );
            arguments.parallel = true;
            block.push(arguments.into());
        }
        block.push(
            ast::Assign::new(
                vec![ast::LValue::Local(state.clone())],
                vec![ast::Literal::Number(target.index() as f64).into()],
            )
            .into(),
        );
    }

    fn lower_dispatch_pseudo_statements(block: &mut ast::Block) {
        let statements = std::mem::take(&mut block.0);
        block.0 = statements
            .into_iter()
            .map(|statement| match statement {
                ast::Statement::NumForInit(init) => {
                    let mut assignment = ast::Assign::new(
                        vec![init.counter.0, init.limit.0, init.step.0],
                        vec![
                            ast::Binary::new(
                                init.counter.1,
                                init.step.1.clone(),
                                ast::BinaryOperation::Sub,
                            )
                            .into(),
                            init.limit.1,
                            init.step.1,
                        ],
                    );
                    assignment.parallel = true;
                    assignment.into()
                }
                ast::Statement::GenericForInit(init) => ast::Statement::Assign(init.0),
                other => other,
            })
            .collect();
    }

    fn numeric_for_dispatch_condition(next: &ast::NumForNext) -> ast::RValue {
        let counter = next
            .counter
            .0
            .as_local()
            .expect("numeric-for counter must be a local")
            .clone();
        let positive_step = ast::Binary::new(
            next.step.clone(),
            ast::Literal::Number(0.0).into(),
            ast::BinaryOperation::GreaterThan,
        );
        let positive_limit = ast::Binary::new(
            counter.clone().into(),
            next.limit.clone(),
            ast::BinaryOperation::LessThanOrEqual,
        );
        let nonpositive_step = ast::Binary::new(
            next.step.clone(),
            ast::Literal::Number(0.0).into(),
            ast::BinaryOperation::LessThanOrEqual,
        );
        let nonpositive_limit = ast::Binary::new(
            counter.into(),
            next.limit.clone(),
            ast::BinaryOperation::GreaterThanOrEqual,
        );
        ast::Binary::new(
            ast::Binary::new(
                positive_step.into(),
                positive_limit.into(),
                ast::BinaryOperation::And,
            )
            .into(),
            ast::Binary::new(
                nonpositive_step.into(),
                nonpositive_limit.into(),
                ast::BinaryOperation::And,
            )
            .into(),
            ast::BinaryOperation::Or,
        )
        .into()
    }

    fn dispatch_block_terminates(block: &ast::Block) -> bool {
        block
            .iter()
            .rev()
            .find(|statement| {
                !matches!(
                    statement,
                    ast::Statement::Comment(_) | ast::Statement::Empty(_)
                )
            })
            .is_some_and(|statement| match statement {
                ast::Statement::Return(_)
                | ast::Statement::Break(_)
                | ast::Statement::Continue(_)
                | ast::Statement::Goto(_) => true,
                ast::Statement::If(conditional) => {
                    let then_block = conditional.then_block.lock();
                    let else_block = conditional.else_block.lock();
                    !else_block.is_empty()
                        && Self::dispatch_block_terminates(&then_block)
                        && Self::dispatch_block_terminates(&else_block)
                }
                _ => false,
            })
    }

    fn append_dispatch_exit(block: &mut ast::Block) {
        if !Self::dispatch_block_terminates(block) {
            block.push(ast::Break {}.into());
        }
    }

    fn take_dispatch_conditional(
        block: &mut ast::Block,
        function_id: usize,
        node: NodeIndex,
    ) -> ast::If {
        match block.pop() {
            Some(ast::Statement::If(conditional)) => conditional,
            Some(ast::Statement::NumForNext(next)) => {
                block.push(
                    ast::Assign::new(
                        vec![next.counter.0.clone()],
                        vec![
                            ast::Binary::new(
                                next.counter.1.clone(),
                                next.step.clone(),
                                ast::BinaryOperation::Add,
                            )
                            .into(),
                        ],
                    )
                    .into(),
                );
                ast::If::new(
                    Self::numeric_for_dispatch_condition(&next),
                    ast::Block::default(),
                    ast::Block::default(),
                )
            }
            Some(ast::Statement::GenericForNext(next)) => {
                let control = next
                    .res_locals
                    .first()
                    .and_then(ast::LValue::as_local)
                    .expect("generic-for control result must be a local")
                    .clone();
                let mut results = ast::Assign::new(
                    next.res_locals,
                    vec![
                        ast::Call::new(next.generator, vec![next.state, next.internal_control.1])
                            .into(),
                    ],
                );
                results.parallel = true;
                block.push(results.into());
                block.push(
                    ast::Assign::new(vec![next.internal_control.0], vec![control.clone().into()])
                        .into(),
                );
                ast::If::new(
                    ast::Binary::new(
                        control.into(),
                        ast::Literal::Nil.into(),
                        ast::BinaryOperation::NotEqual,
                    )
                    .into(),
                    ast::Block::default(),
                    ast::Block::default(),
                )
            }
            tail => {
                panic!(
                    "function {function_id} conditional dispatcher node {} has no branch condition; tail={:?}",
                    node.index(),
                    tail.map(|statement| std::mem::discriminant(&statement))
                );
            }
        }
    }

    fn detach_nested_blocks(block: &mut ast::Block) {
        for statement in &mut block.0 {
            let nested = match statement {
                ast::Statement::If(conditional) => {
                    let mut then_block = conditional.then_block.lock().clone();
                    let mut else_block = conditional.else_block.lock().clone();
                    Self::detach_nested_blocks(&mut then_block);
                    Self::detach_nested_blocks(&mut else_block);
                    conditional.then_block = Arc::new(Mutex::new(then_block));
                    conditional.else_block = Arc::new(Mutex::new(else_block));
                    continue;
                }
                ast::Statement::Do(scope) => &mut scope.block,
                ast::Statement::While(r#while) => &mut r#while.block,
                ast::Statement::Repeat(repeat) => &mut repeat.block,
                ast::Statement::NumericFor(numeric_for) => &mut numeric_for.block,
                ast::Statement::GenericFor(generic_for) => &mut generic_for.block,
                _ => continue,
            };
            let mut nested_block = nested.lock().clone();
            Self::detach_nested_blocks(&mut nested_block);
            *nested = Arc::new(Mutex::new(nested_block));
        }
    }

    fn detached_function_clone(function: &Function) -> Function {
        let mut detached = function.clone();
        for block in detached.blocks_mut() {
            Self::detach_nested_blocks(block);
        }
        detached
    }

    fn contains_invalid_structure(block: &ast::Block, loop_depth: usize) -> bool {
        let mut terminated = false;
        for statement in &block.0 {
            if matches!(
                statement,
                ast::Statement::Comment(_) | ast::Statement::Empty(_)
            ) {
                continue;
            }
            if terminated {
                return true;
            }
            let invalid = match statement {
                ast::Statement::Break(_) | ast::Statement::Continue(_) => loop_depth == 0,
                ast::Statement::Goto(_)
                | ast::Statement::Label(_)
                | ast::Statement::NumForInit(_)
                | ast::Statement::NumForNext(_)
                | ast::Statement::GenericForInit(_)
                | ast::Statement::GenericForNext(_) => true,
                ast::Statement::If(conditional) => {
                    Self::contains_invalid_structure(&conditional.then_block.lock(), loop_depth)
                        || Self::contains_invalid_structure(
                            &conditional.else_block.lock(),
                            loop_depth,
                        )
                }
                ast::Statement::Do(scope) => {
                    Self::contains_invalid_structure(&scope.block.lock(), loop_depth)
                }
                ast::Statement::While(r#while) => {
                    Self::contains_invalid_structure(&r#while.block.lock(), loop_depth + 1)
                }
                ast::Statement::Repeat(repeat) => {
                    Self::contains_invalid_structure(&repeat.block.lock(), loop_depth + 1)
                }
                ast::Statement::NumericFor(numeric_for) => {
                    Self::contains_invalid_structure(&numeric_for.block.lock(), loop_depth + 1)
                }
                ast::Statement::GenericFor(generic_for) => {
                    Self::contains_invalid_structure(&generic_for.block.lock(), loop_depth + 1)
                }
                _ => false,
            };
            if invalid {
                return true;
            }
            terminated = matches!(
                statement,
                ast::Statement::Return(_) | ast::Statement::Break(_) | ast::Statement::Continue(_)
            );
        }
        false
    }

    fn dispatch_tree(state: &ast::RcLocal, mut cases: Vec<(NodeIndex, ast::Block)>) -> ast::Block {
        match cases.len() {
            0 => ast::Block(vec![ast::Break {}.into()]),
            1 => {
                let (node, block) = cases.pop().unwrap();
                let condition = ast::Binary::new(
                    state.clone().into(),
                    ast::Literal::Number(node.index() as f64).into(),
                    ast::BinaryOperation::Equal,
                )
                .into();
                ast::Block(vec![
                    ast::If::new(condition, block, ast::Block(vec![ast::Break {}.into()])).into(),
                ])
            }
            _ => {
                let right = cases.split_off(cases.len() / 2);
                let pivot = cases.last().unwrap().0;
                let condition = ast::Binary::new(
                    state.clone().into(),
                    ast::Literal::Number(pivot.index() as f64).into(),
                    ast::BinaryOperation::LessThanOrEqual,
                )
                .into();
                ast::Block(vec![
                    ast::If::new(
                        condition,
                        Self::dispatch_tree(state, cases),
                        Self::dispatch_tree(state, right),
                    )
                    .into(),
                ])
            }
        }
    }

    /// Lowers any graph that cannot be expressed with Lua 5.1's structured
    /// statements to a bounded state machine. A balanced decision tree keeps
    /// later AST passes and runtime dispatch logarithmic in the block count.
    /// Original edge assignments and branch conditions remain intact without
    /// emitting Lua 5.2 gotos.
    fn dispatch_remaining_cfg(&mut self) -> ast::Block {
        let entry = self.function.entry().unwrap();
        let reachable = Dfs::new(self.function.graph(), entry)
            .iter(self.function.graph())
            .collect::<FxHashSet<_>>();
        let mut nodes = reachable.into_iter().collect::<Vec<_>>();
        nodes.sort_by_key(|node| node.index());

        let mut blocks = Vec::with_capacity(nodes.len());
        for node in nodes {
            let edges = self
                .function
                .edges(node)
                .map(|edge| (edge.target(), edge.weight().clone()))
                .collect::<Vec<_>>();
            let mut block = self.function.remove_block(node).unwrap();
            Self::lower_dispatch_pseudo_statements(&mut block);
            blocks.push((node, block, edges));
        }

        let state = ast::RcLocal::default();
        let mut cases = Vec::with_capacity(blocks.len());
        for (node, mut block, edges) in blocks {
            match edges.as_slice() {
                [] => Self::append_dispatch_exit(&mut block),
                [(target, edge)] => match edge.branch_type {
                    BranchType::Unconditional => {
                        Self::append_dispatch_transfer(&mut block, &state, *target, edge);
                    }
                    BranchType::Then | BranchType::Else => {
                        let conditional =
                            Self::take_dispatch_conditional(&mut block, self.function.id, node);
                        let (taken, missing) = if edge.branch_type == BranchType::Then {
                            (&conditional.then_block, &conditional.else_block)
                        } else {
                            (&conditional.else_block, &conditional.then_block)
                        };
                        Self::append_dispatch_transfer(&mut taken.lock(), &state, *target, edge);
                        Self::append_dispatch_exit(&mut missing.lock());
                        block.push(conditional.into());
                    }
                },
                [first, second] => {
                    let (then_edge, else_edge) = match (&first.1.branch_type, &second.1.branch_type)
                    {
                        (BranchType::Then, BranchType::Else) => (first, second),
                        (BranchType::Else, BranchType::Then) => (second, first),
                        _ => panic!("conditional dispatcher node must have then and else edges"),
                    };
                    let conditional =
                        Self::take_dispatch_conditional(&mut block, self.function.id, node);
                    Self::append_dispatch_transfer(
                        &mut conditional.then_block.lock(),
                        &state,
                        then_edge.0,
                        &then_edge.1,
                    );
                    Self::append_dispatch_transfer(
                        &mut conditional.else_block.lock(),
                        &state,
                        else_edge.0,
                        &else_edge.1,
                    );
                    block.push(conditional.into());
                }
                _ => panic!("dispatcher node has more than two successors"),
            }

            cases.push((node, block));
        }
        let cases = Self::dispatch_tree(&state, cases);

        let mut initialize = ast::Assign::new(
            vec![ast::LValue::Local(state)],
            vec![ast::Literal::Number(entry.index() as f64).into()],
        );
        initialize.prefix = true;
        ast::Block(vec![
            initialize.into(),
            ast::While::new(ast::Literal::Boolean(true).into(), cases).into(),
        ])
    }

    fn structure(mut self) -> ast::Block {
        let mut collapsed_fallback = None;
        let mut result = if Self::collapse_within_budget(self.function.graph().node_count()) {
            let fallback_function = Self::detached_function_clone(&self.function);
            if self.collapse() {
                collapsed_fallback = Some(fallback_function);
                Self::remove_last_return(
                    self.function
                        .remove_block(self.function.entry().unwrap())
                        .unwrap(),
                )
            } else {
                self.function = fallback_function;
                self.dispatch_remaining_cfg()
            }
        } else {
            self.dispatch_remaining_cfg()
        };
        // Loop exits first: an unrecovered `goto` in the interior disqualifies
        // the whole block from terminal back-edge recovery below.
        recover_loop_exit_breaks(&mut result);
        let mut referenced = FxHashSet::default();
        collect_referenced_labels(&result, &mut referenced);
        remove_unreferenced_labels(&mut result, &referenced);
        recover_terminal_backedge_loop(&mut result);
        flatten_single_iteration_loops(&mut result);
        relocate_unreachable_terminal_returns(&mut result, &self.reachable_terminal_returns);
        referenced.clear();
        collect_referenced_labels(&result, &mut referenced);
        remove_unreferenced_labels(&mut result, &referenced);
        if Self::contains_invalid_structure(&result, 0)
            && let Some(fallback_function) = collapsed_fallback
        {
            self.function = fallback_function;
            result = self.dispatch_remaining_cfg();
            recover_loop_exit_breaks(&mut result);
            referenced.clear();
            collect_referenced_labels(&result, &mut referenced);
            remove_unreferenced_labels(&mut result, &referenced);
            recover_terminal_backedge_loop(&mut result);
            flatten_single_iteration_loops(&mut result);
            relocate_unreachable_terminal_returns(&mut result, &self.reachable_terminal_returns);
            referenced.clear();
            collect_referenced_labels(&result, &mut referenced);
            remove_unreferenced_labels(&mut result, &referenced);
        }
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
            ast::Statement::Do(r#do) => {
                changed |= replace_exit_gotos_with_break(&mut r#do.block.lock(), label);
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
            ast::Statement::Do(r#do) => {
                changed |= recover_loop_exit_breaks(&mut r#do.block.lock());
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
        let Some(label) = block
            .0
            .get(index + 1)
            .and_then(ast::Statement::as_label)
            .cloned()
        else {
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
            ast::Statement::Do(r#do) => {
                collect_referenced_labels(&r#do.block.lock(), referenced);
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
            ast::Statement::Do(r#do) => {
                changed |= remove_unreferenced_labels(&mut r#do.block.lock(), referenced);
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
        ast::Statement::Do(do_) => contains_unstructured_jump(&do_.block.lock()),
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
        GraphStructurer, collect_referenced_labels, contains_reachable_return,
        contains_unstructured_jump, flatten_single_iteration_loops, lift, recover_loop_exit_breaks,
        recover_terminal_backedge_loop, relocate_unreachable_terminal_returns,
        remove_unreferenced_labels,
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

    fn maximum_if_depth(block: &ast::Block) -> usize {
        block
            .iter()
            .map(|statement| match statement {
                Statement::If(r#if) => {
                    1 + maximum_if_depth(&r#if.then_block.lock())
                        .max(maximum_if_depth(&r#if.else_block.lock()))
                }
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn dispatcher_uses_a_balanced_decision_tree() {
        let state = local("state");
        let cases = (0..4096)
            .map(|index| {
                (
                    petgraph::stable_graph::NodeIndex::new(index),
                    ast::Block(vec![ast::Break {}.into()]),
                )
            })
            .collect();

        let tree = GraphStructurer::dispatch_tree(&state, cases);

        assert!(maximum_if_depth(&tree) <= 13);
    }

    #[test]
    fn dispatcher_does_not_append_break_after_return() {
        let mut terminal = ast::Block(vec![Return::new(Vec::new()).into()]);
        GraphStructurer::append_dispatch_exit(&mut terminal);

        assert_eq!(terminal.len(), 1);
        assert!(matches!(terminal[0], Statement::Return(_)));

        let mut nonterminal = ast::Block(vec![
            Call::new(Global::from("work").into(), Vec::new()).into(),
        ]);
        GraphStructurer::append_dispatch_exit(&mut nonterminal);

        assert!(matches!(nonterminal.last(), Some(Statement::Break(_))));
    }

    #[test]
    fn dispatcher_lowers_generic_for_next_without_a_pseudo_node() {
        let generator = local("generator");
        let state = local("state");
        let internal_control = local("internal");
        let item = local("item");
        let mut block = ast::Block(vec![
            ast::GenericForNext::new(vec![item], generator.into(), state, internal_control).into(),
        ]);

        let conditional = GraphStructurer::take_dispatch_conditional(
            &mut block,
            0,
            petgraph::stable_graph::NodeIndex::new(0),
        );
        block.push(conditional.into());

        assert_eq!(block.len(), 3);
        assert!(matches!(block[0], Statement::Assign(_)));
        assert!(matches!(block[1], Statement::Assign(_)));
        assert!(matches!(block[2], Statement::If(_)));
        assert!(!GraphStructurer::contains_invalid_structure(&block, 0));
        let source = ast::format_lua51(&block);
        assert!(source.contains("item = generator(state, internal)"));
        assert!(source.contains("internal = item"));
        assert!(source.contains("if item ~= nil then"));
    }

    #[test]
    fn fallback_clone_detaches_nested_blocks() {
        let mut function = Function::new(0);
        let entry = function.new_block();
        function.set_entry(entry);
        function.block_mut(entry).unwrap().push(
            If::new(
                Literal::Boolean(true).into(),
                ast::Block::default(),
                ast::Block::default(),
            )
            .into(),
        );

        let mut detached = GraphStructurer::detached_function_clone(&function);
        detached.block_mut(entry).unwrap()[0]
            .as_if_mut()
            .unwrap()
            .then_block
            .lock()
            .push(Return::new(Vec::new()).into());

        assert!(
            function.block(entry).unwrap()[0]
                .as_if()
                .unwrap()
                .then_block
                .lock()
                .is_empty()
        );
    }

    #[test]
    fn invalid_structure_tracks_loop_scope_and_terminal_tails() {
        assert!(GraphStructurer::contains_invalid_structure(
            &ast::Block(vec![ast::Break {}.into()]),
            0,
        ));
        assert!(!GraphStructurer::contains_invalid_structure(
            &ast::Block(vec![
                ast::While::new(
                    Literal::Boolean(true).into(),
                    ast::Block(vec![ast::Break {}.into()]),
                )
                .into(),
            ]),
            0,
        ));
        assert!(GraphStructurer::contains_invalid_structure(
            &ast::Block(vec![
                Return::new(Vec::new()).into(),
                Call::new(Global::from("unreachable").into(), Vec::new()).into(),
            ]),
            0,
        ));
    }

    #[test]
    fn quadratic_collapse_work_is_bounded() {
        assert!(GraphStructurer::collapse_within_budget(10_000));
        assert!(!GraphStructurer::collapse_within_budget(10_001));
        assert!(!GraphStructurer::collapse_within_budget(usize::MAX));
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
    fn irreducible_graph_uses_lua51_dispatcher_without_gotos() {
        let mut function = Function::new(0);
        let entry = function.new_block();
        let left = function.new_block();
        let right = function.new_block();
        let join = function.new_block();
        function.set_entry(entry);

        function.block_mut(entry).unwrap().push(
            If::new(
                Call::new(Global::from("chooseEntry").into(), Vec::new()).into(),
                Default::default(),
                Default::default(),
            )
            .into(),
        );
        function
            .block_mut(left)
            .unwrap()
            .push(Call::new(Global::from("left").into(), Vec::new()).into());
        function
            .block_mut(right)
            .unwrap()
            .push(Call::new(Global::from("right").into(), Vec::new()).into());
        function.block_mut(join).unwrap().push(
            If::new(
                Call::new(Global::from("chooseLoop").into(), Vec::new()).into(),
                Default::default(),
                Default::default(),
            )
            .into(),
        );

        function
            .graph_mut()
            .add_edge(entry, left, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(entry, right, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(left, join, BlockEdge::new(BranchType::Unconditional));
        function
            .graph_mut()
            .add_edge(right, join, BlockEdge::new(BranchType::Unconditional));
        function
            .graph_mut()
            .add_edge(join, left, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(join, right, BlockEdge::new(BranchType::Else));

        let facts = RecoveryFacts::derive(&function).unwrap();
        let block = lift(function, &facts);

        assert!(!contains_unstructured_jump(&block));
        assert!(block.iter().any(|statement| statement.as_while().is_some()));
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
            ast::While::new(
                Literal::Boolean(true).into(),
                ast::Block(vec![inner.into()]),
            )
            .into(),
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
