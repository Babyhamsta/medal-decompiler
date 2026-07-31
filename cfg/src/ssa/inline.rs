use crate::function::Function;
use ast::{LocalRw, Reduce, SideEffects, Traverse};
use indexmap::IndexMap;
use itertools::{Either, Itertools};
use petgraph::visit::EdgeRef;
use rustc_hash::{FxHashMap, FxHashSet};

struct TraverseSelf<'a, T: Traverse>(&'a mut T);

impl<'a> Traverse for TraverseSelf<'a, ast::RValue> {
    fn rvalues_mut(&mut self) -> ast::RValueRefsMut<'_> {
        smallvec::smallvec![&mut *self.0]
    }

    fn rvalues(&self) -> ast::RValueRefs<'_> {
        smallvec::smallvec![&*self.0]
    }
}

struct Inliner<'a> {
    function: &'a mut Function,
    local_to_group: &'a FxHashMap<ast::RcLocal, usize>,
    upvalue_to_group: &'a IndexMap<ast::RcLocal, ast::RcLocal>,
    local_usages: &'a mut FxHashMap<ast::RcLocal, usize>,
    /// Set at each site that rewrites a statement or edge argument, so the
    /// caller can report a change without fingerprinting the whole function.
    changed: bool,
}

fn has_open_table_tail(table: &ast::Table) -> bool {
    table.0.last().is_some_and(|(key, value)| {
        key.is_none()
            && matches!(
                value,
                ast::RValue::VarArg(_) | ast::RValue::Call(_) | ast::RValue::MethodCall(_)
            )
    })
}

fn fold_table_fields(
    block: &mut ast::Block,
    effect_observable_locals: &FxHashSet<ast::RcLocal>,
) -> usize {
    let mut folded = 0;
    let mut index = 0;
    while index < block.len() {
        let Some((table_index, object_local)) = block[index]
            .as_assign()
            .filter(|assign| {
                !assign.parallel
                    && assign.left.len() == 1
                    && assign.right.len() == 1
                    && assign
                        .right
                        .first()
                        .and_then(ast::RValue::as_table)
                        .is_some_and(|table| !has_open_table_tail(table))
            })
            .and_then(|assign| {
                assign.left[0]
                    .as_local()
                    .cloned()
                    .map(|local| (index, local))
            })
        else {
            index += 1;
            continue;
        };

        index += 1;
        while index < block.len() {
            let Some((key, value)) = block[index]
                .as_assign()
                .filter(|assign| {
                    !assign.prefix
                        && !assign.parallel
                        && assign.left.len() == 1
                        && assign.right.len() == 1
                })
                .and_then(|assign| {
                    let index = assign.left[0].as_index()?;
                    let ast::RValue::Local(local) = index.left.as_ref() else {
                        return None;
                    };
                    (local == &object_local)
                        .then(|| (index.right.as_ref().clone(), assign.right[0].clone()))
                })
            else {
                break;
            };

            if (effect_observable_locals.contains(&object_local)
                && (key.has_side_effects() || value.has_side_effects()))
                || key.values_read().contains(&&object_local)
                || value.values_read().contains(&&object_local)
            {
                break;
            }

            block[table_index].as_assign_mut().unwrap().right[0]
                .as_table_mut()
                .unwrap()
                .0
                .push((Some(key), value));
            block.remove(index);
            folded += 1;
        }
    }
    folded
}

fn fold_set_lists(
    block: &mut ast::Block,
    local_usages: &mut FxHashMap<ast::RcLocal, usize>,
) -> usize {
    let mut folded = 0;
    for set_index in 0..block.len() {
        let ast::Statement::SetList(set_list) = block[set_index].clone() else {
            continue;
        };
        let Some(expected_values) = set_list.index.checked_sub(1) else {
            continue;
        };

        let mut search_before = set_index;
        let mut defined_local = set_list.object_local.clone();
        let mut aliases = Vec::new();
        let table_index = loop {
            let Some(definition_index) = (0..search_before)
                .rev()
                .find(|&index| block[index].values_written().contains(&&defined_local))
            else {
                break None;
            };
            let Some(assign) = block[definition_index].as_assign() else {
                break None;
            };
            if assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
                break None;
            }
            let Some(target) = assign.left[0].as_local() else {
                break None;
            };
            if target != &defined_local {
                break None;
            }
            if assign.right[0].as_table().is_some() {
                break Some(definition_index);
            }
            let Some(source) = assign.right[0].as_local() else {
                break None;
            };
            if local_usages.get(&defined_local) != Some(&1) {
                break None;
            }
            aliases.push((definition_index, defined_local, source.clone()));
            defined_local = source.clone();
            search_before = definition_index;
        };
        let Some(table_index) = table_index else {
            continue;
        };

        let table = block[table_index].as_assign().unwrap().right[0]
            .as_table()
            .unwrap();
        if has_open_table_tail(table)
            || table.0.iter().filter(|(key, _)| key.is_none()).count() != expected_values
        {
            continue;
        }
        let mut protected_locals = aliases
            .iter()
            .map(|(_, target, _)| target.clone())
            .collect::<Vec<_>>();
        protected_locals.push(defined_local.clone());
        let expressions_read_crossed_local = set_list
            .values
            .iter()
            .chain(set_list.tail.as_ref())
            .flat_map(ast::LocalRw::values_read)
            .any(|read| protected_locals.contains(read));
        if expressions_read_crossed_local
            || !local_usages
                .get(&set_list.object_local)
                .is_some_and(|usage| *usage > 0)
        {
            continue;
        }

        let alias_indices = aliases
            .iter()
            .map(|(index, _, _)| *index)
            .collect::<FxHashSet<_>>();
        let can_keep_constructor_in_place = (table_index + 1..set_index)
            .all(|index| alias_indices.contains(&index) || block[index].as_empty().is_some());
        if !can_keep_constructor_in_place {
            let table_values_are_movable = table.0.iter().all(|(key, value)| {
                key.as_ref().is_none_or(|key| !key.has_side_effects()) && !value.has_side_effects()
            }) && table.values_read().is_empty();
            let table_is_unobserved = (table_index + 1..set_index)
                .filter(|index| !alias_indices.contains(index))
                .all(|index| {
                    !block[index]
                        .values_read()
                        .iter()
                        .any(|read| protected_locals.contains(read))
                        && !block[index]
                            .values_written()
                            .iter()
                            .any(|written| protected_locals.contains(written))
                });
            if !table_values_are_movable || !table_is_unobserved {
                continue;
            }
        }

        let mut table_assign = block[table_index].as_assign().unwrap().clone();
        let table = table_assign.right[0].as_table_mut().unwrap();
        table
            .0
            .extend(set_list.values.into_iter().map(|value| (None, value)));
        if let Some(tail) = set_list.tail {
            table.0.push((None, tail));
        }
        *local_usages.get_mut(&set_list.object_local).unwrap() -= 1;

        if can_keep_constructor_in_place {
            block[table_index] = table_assign.into();
            block[set_index] = ast::Empty {}.into();
        } else {
            block[table_index] = ast::Empty {}.into();
            for (alias_index, _, source) in aliases {
                block[alias_index] = ast::Empty {}.into();
                *local_usages.get_mut(&source).unwrap() -= 1;
            }
            block[set_index] = table_assign.into();
        }
        folded += 1;
    }
    folded
}

impl<'a> Inliner<'a> {
    fn new(
        function: &'a mut Function,
        local_to_group: &'a FxHashMap<ast::RcLocal, usize>,
        upvalue_to_group: &'a IndexMap<ast::RcLocal, ast::RcLocal>,
        local_usages: &'a mut FxHashMap<ast::RcLocal, usize>,
    ) -> Self {
        Self {
            function,
            local_to_group,
            upvalue_to_group,
            local_usages,
            changed: false,
        }
    }

    fn try_inline(
        traversible: &mut impl Traverse,
        read: &ast::RcLocal,
        new_rvalue: &mut Option<ast::RValue>,
        new_rvalue_has_side_effects: bool,
    ) -> bool {
        traversible
            .traverse_values(&mut |p, v| {
                match p {
                    ast::PreOrPost::Pre => {
                        if let Either::Right(rvalue) = v {
                            match rvalue {
                                ast::RValue::Binary(ast::Binary {
                                    left,
                                    right,
                                    operation,
                                }) if operation.is_comparator()
                                    && left.has_side_effects()
                                    && let &mut box ast::RValue::Local(ref local) = right
                                    && local == read =>
                                {
                                    *right = std::mem::replace(
                                        left,
                                        Box::new(new_rvalue.take().unwrap()),
                                    );
                                    *operation = match *operation {
                                        // TODO: __eq metamethod?
                                        ast::BinaryOperation::Equal => ast::BinaryOperation::Equal,
                                        ast::BinaryOperation::NotEqual => {
                                            ast::BinaryOperation::NotEqual
                                        }
                                        ast::BinaryOperation::LessThanOrEqual => {
                                            ast::BinaryOperation::GreaterThanOrEqual
                                        }
                                        ast::BinaryOperation::GreaterThanOrEqual => {
                                            ast::BinaryOperation::LessThanOrEqual
                                        }
                                        ast::BinaryOperation::LessThan => {
                                            ast::BinaryOperation::GreaterThan
                                        }
                                        ast::BinaryOperation::GreaterThan => {
                                            ast::BinaryOperation::LessThan
                                        }
                                        _ => unreachable!(),
                                    };
                                    return Some(true);
                                }
                                _ => {}
                            }
                        }
                    }
                    ast::PreOrPost::Post => {
                        if let Either::Right(rvalue) = v {
                            match rvalue {
                                ast::RValue::Local(local) if local == read => {
                                    *rvalue = new_rvalue.take().unwrap();
                                    // success!
                                    return Some(true);
                                }
                                _ => {}
                            }
                            if new_rvalue_has_side_effects && rvalue.has_side_effects() {
                                // failure :(
                                return Some(false);
                            }
                        }
                    }
                }
                // keep searching
                None
            })
            .unwrap_or(false)
    }

    // TODO: dont clone rvalues
    // TODO: REFACTOR: move to ssa module?
    // TODO: inline into block arguments
    /// Returns whether any statement or edge argument was rewritten.
    fn inline_rvalues(mut self) -> bool {
        let node_indices = self.function.graph().node_indices().collect::<Vec<_>>();
        for node in node_indices {
            let block = self.function.block_mut(node).unwrap();

            // TODO: rename values_read to locals_read
            let mut stat_to_values_read = Vec::with_capacity(block.len());
            for stat in &block.0 {
                stat_to_values_read.push(
                    stat.values_read()
                        .into_iter()
                        .filter(|&l| {
                            self.local_usages[l] == 1 && !self.upvalue_to_group.contains_key(l)
                        })
                        .cloned()
                        .map(Some)
                        .collect_vec(),
                );
            }

            // visit all statements that read at least one local with only one usage,
            // this is the statement we will inline into
            // then seek backwards from the previous statement to the start of the block
            // until we find a statement that assigns to a single-use local that
            // is used in the statement we are inlining into.
            // TODO: push multiple use local assignments forward to their first use
            let mut index = 0;
            'w: while index < block.len() {
                let mut groups_written = FxHashSet::default();
                let mut allow_side_effects = true;
                for stat_index in (0..index).rev() {
                    let mut values_read = stat_to_values_read[index]
                        .iter_mut()
                        .filter(|l| l.is_some())
                        .peekable();
                    if values_read.peek().is_none() {
                        index += 1;
                        continue 'w;
                    }
                    // we cant inline across upvalue writes because an inlining candidate with side effects,
                    // for ex. a non-local function call, might access the upvalue
                    for value_written in block[stat_index].values_written() {
                        if self.upvalue_to_group.contains_key(value_written) {
                            // TODO: set allow_side_effects to false instead
                            allow_side_effects = false;
                        }
                    }

                    /*
                    -- we dont want to inline `tostring(a)` into `print(b)`
                    local print = print
                    local a = 1
                    while true do
                        local b = tostring(a)
                        a = 1
                        print(b)
                    end
                    */
                    if block[stat_index]
                        .values_read()
                        .into_iter()
                        .filter_map(|l| self.local_to_group.get(l))
                        .any(|g| groups_written.contains(g))
                    {
                        continue;
                    }

                    if let ast::Statement::Assign(assign) = &block[stat_index]
                        && let Ok(new_rvalue) = assign.right.iter().exactly_one()
                    {
                        let new_rvalue_has_side_effects = new_rvalue.has_side_effects()
                            || new_rvalue
                                .values_read()
                                .iter()
                                .any(|v| self.upvalue_to_group.contains_key(*v));
                        if !new_rvalue_has_side_effects || allow_side_effects {
                            if let Ok(ast::LValue::Local(local)) = &assign.left.iter().exactly_one()
                                && let Some(read) = stat_to_values_read[index]
                                    .iter_mut()
                                    .find(|l| l.as_ref() == Some(local))
                            {
                                let mut new_rvalue = Some(
                                    block[stat_index]
                                        .as_assign_mut()
                                        .unwrap()
                                        .right
                                        .pop()
                                        .unwrap(),
                                );
                                if Self::try_inline(
                                    &mut block[index],
                                    read.as_ref().unwrap(),
                                    &mut new_rvalue,
                                    new_rvalue_has_side_effects,
                                ) {
                                    assert!(new_rvalue.is_none());

                                    // TODO: PERF: this is probably inefficient
                                    for rvalue in block[index].rvalues_mut() {
                                        *rvalue =
                                            std::mem::replace(rvalue, ast::Literal::Nil.into())
                                                .reduce();
                                    }

                                    // TODO: PERF: remove `local_usages[l] == 1` filter in stat_to_values_read
                                    // and use stat_to_values_read here
                                    for local in block[stat_index].values_read() {
                                        let local_usage_count =
                                            self.local_usages.get_mut(local).unwrap();
                                        *local_usage_count = local_usage_count.saturating_sub(1);
                                    }
                                    // we dont need to update local usages because tracking usages for a local
                                    // with no declarations serves no purpose
                                    block[stat_index] = ast::Empty {}.into();
                                    *read = None;
                                    self.changed = true;
                                    continue 'w;
                                } else {
                                    block[stat_index]
                                        .as_assign_mut()
                                        .unwrap()
                                        .right
                                        .push(new_rvalue.unwrap());
                                }
                            } else if let Some(generic_for_init) =
                                block[index].as_generic_for_init()
                                && generic_for_init
                                    .0
                                    .right
                                    .iter()
                                    .rev()
                                    .map_while(|r| r.as_local())
                                    .eq_by(assign.left.iter().rev(), |a, b| Some(a) == b.as_local())
                                && assign.left.iter().all(|l| {
                                    l.as_local().is_some_and(|l| {
                                        stat_to_values_read[index]
                                            .iter_mut()
                                            .any(|r| r.as_ref() == Some(l))
                                    })
                                })
                            {
                                let start_index =
                                    generic_for_init.0.right.len() - assign.left.len();
                                let has_leading_side_effects = || {
                                    let mut leading_side_effects = false;
                                    for expr in generic_for_init.0.right.iter().take(start_index) {
                                        if expr.has_side_effects() {
                                            leading_side_effects = true;
                                            break;
                                        }
                                    }
                                    leading_side_effects
                                };

                                if !new_rvalue_has_side_effects || !has_leading_side_effects() {
                                    let new_rvalue = block[stat_index]
                                        .as_assign_mut()
                                        .unwrap()
                                        .right
                                        .pop()
                                        .unwrap();

                                    let generic_for_init =
                                        block[index].as_generic_for_init_mut().unwrap();
                                    let old_locals = generic_for_init
                                        .0
                                        .right
                                        .drain(start_index..)
                                        .map(|r| r.as_local().unwrap().clone())
                                        .collect_vec();
                                    generic_for_init.0.right.push(new_rvalue);

                                    // TODO: PERF: remove `local_usages[l] == 1` filter in stat_to_values_read
                                    // and use stat_to_values_read here
                                    for local in block[stat_index].values_read() {
                                        let local_usage_count =
                                            self.local_usages.get_mut(local).unwrap();
                                        *local_usage_count = local_usage_count.saturating_sub(1);
                                    }
                                    // we dont need to update local usages because tracking usages for a local
                                    // with no declarations serves no purpose
                                    block[stat_index] = ast::Empty {}.into();
                                    for old_local in old_locals {
                                        *stat_to_values_read[index]
                                            .iter_mut()
                                            .find(|l| l.as_ref() == Some(&old_local))
                                            .unwrap() = None;
                                    }
                                    self.changed = true;
                                    continue 'w;
                                }
                            }
                        }
                    }
                    groups_written.extend(
                        block[stat_index]
                            .values_written()
                            .into_iter()
                            .filter_map(|l| self.local_to_group.get(l))
                            .cloned(),
                    );
                    allow_side_effects &= !block[stat_index].has_side_effects();
                }
                index += 1;
            }
            // we cant inline anything with side effects or anything that depends on other params
            // because block params are executed in parallel.
            for edge in self.function.edges(node).map(|e| e.id()).collect_vec() {
                // TODO: rename values_read to locals_read
                let mut arg_to_values_read = self
                    .function
                    .graph()
                    .edge_weight(edge)
                    .unwrap()
                    .arguments
                    .iter()
                    .map(|(_, a)| {
                        a.values_read()
                            .into_iter()
                            .filter(|&l| {
                                self.local_usages[l] == 1 && !self.upvalue_to_group.contains_key(l)
                            })
                            .cloned()
                            .map(Some)
                            .collect_vec()
                    })
                    .collect_vec();

                let mut index = 0;
                'w: while index < arg_to_values_read.len() {
                    let mut groups_written = FxHashSet::default();
                    for stat_index in (0..self.function.block(node).unwrap().len()).rev() {
                        let mut values_read = arg_to_values_read[index]
                            .iter_mut()
                            .filter(|l| l.is_some())
                            .peekable();
                        if values_read.peek().is_none() {
                            index += 1;
                            continue 'w;
                        }
                        let block = self.function.block_mut(node).unwrap();
                        // we cant inline across upvalue writes because an inlining candidate with side effects,
                        // for ex. a non-local function call, might access the upvalue
                        for value_written in block[stat_index].values_written() {
                            if self.upvalue_to_group.contains_key(value_written) {
                                // TODO: set allow_side_effects to false instead
                                index += 1;
                                continue 'w;
                            }
                        }

                        /*
                        -- we dont want to inline `tostring(a)` into `print(b)`
                        local print = print
                        local a = 1
                        while true do
                            local b = tostring(a)
                            a = 1
                            print(b)
                        end
                        */
                        if block[stat_index]
                            .values_read()
                            .into_iter()
                            .filter_map(|l| self.local_to_group.get(l))
                            .any(|g| groups_written.contains(g))
                        {
                            continue;
                        }

                        if let ast::Statement::Assign(assign) = &block[stat_index]
                            && let Ok(new_rvalue) = assign.right.iter().exactly_one()
                        {
                            let new_rvalue_has_side_effects = new_rvalue.has_side_effects()
                                || new_rvalue
                                    .values_read()
                                    .iter()
                                    .any(|v| self.upvalue_to_group.contains_key(*v));
                            if !new_rvalue_has_side_effects
                                && let Ok(ast::LValue::Local(local)) =
                                    &assign.left.iter().exactly_one()
                                && let Some(read) = arg_to_values_read[index]
                                    .iter_mut()
                                    .find(|l| l.as_ref() == Some(local))
                            {
                                let mut new_rvalue = Some(
                                    block[stat_index]
                                        .as_assign_mut()
                                        .unwrap()
                                        .right
                                        .pop()
                                        .unwrap(),
                                );
                                if Self::try_inline(
                                    &mut TraverseSelf(
                                        &mut self
                                            .function
                                            .graph_mut()
                                            .edge_weight_mut(edge)
                                            .unwrap()
                                            .arguments[index]
                                            .1,
                                    ),
                                    read.as_ref().unwrap(),
                                    &mut new_rvalue,
                                    new_rvalue_has_side_effects,
                                ) {
                                    assert!(new_rvalue.is_none());
                                    let block = self.function.block_mut(node).unwrap();

                                    // TODO: PERF: remove `local_usages[l] == 1` filter in stat_to_values_read
                                    // and use stat_to_values_read here
                                    for local in block[stat_index].values_read() {
                                        let local_usage_count =
                                            self.local_usages.get_mut(local).unwrap();
                                        *local_usage_count = local_usage_count.saturating_sub(1);
                                    }
                                    // we dont need to update local usages because tracking usages for a local
                                    // with no declarations serves no purpose

                                    block[stat_index] = ast::Empty {}.into();
                                    *read = None;
                                    self.changed = true;
                                    continue 'w;
                                } else {
                                    let block = self.function.block_mut(node).unwrap();

                                    block[stat_index]
                                        .as_assign_mut()
                                        .unwrap()
                                        .right
                                        .push(new_rvalue.unwrap());
                                }
                            }
                        }
                        let block = self.function.block(node).unwrap();

                        groups_written.extend(
                            block[stat_index]
                                .values_written()
                                .into_iter()
                                .filter_map(|l| self.local_to_group.get(l))
                                .cloned(),
                        );
                    }
                    index += 1;
                }
            }
        }
        self.changed
    }
}

/// Returns whether inlining rewrote anything.
///
/// The scheduler needs this to decide whether another round is worthwhile.
/// Reporting it directly lets it skip fingerprinting the whole function before
/// and after the call, which costs two full-AST hashes per round.
pub fn inline(
    function: &mut Function,
    local_to_group: &FxHashMap<ast::RcLocal, usize>,
    upvalue_to_group: &IndexMap<ast::RcLocal, ast::RcLocal>,
) -> bool {
    let mut observable_groups = FxHashSet::default();
    for local in upvalue_to_group.keys().chain(upvalue_to_group.values()) {
        if let Some(group) = local_to_group.get(local) {
            observable_groups.insert(*group);
        }
    }
    let mut effect_observable_locals = upvalue_to_group
        .keys()
        .chain(upvalue_to_group.values())
        .cloned()
        .collect::<FxHashSet<_>>();
    effect_observable_locals.extend(
        local_to_group
            .iter()
            .filter(|(_, group)| observable_groups.contains(group))
            .map(|(local, _)| local.clone()),
    );

    let mut local_usages = FxHashMap::default();
    for node in function.graph().node_indices() {
        for read in function.values_read(node) {
            *local_usages.entry(read.clone()).or_insert(0usize) += 1;
        }
    }

    // `changed` drives loop termination and deliberately excludes
    // `inline_rvalues`, preserving the original fixed point. `any_change` is
    // the reported signal and does include it.
    let mut any_change = false;
    let mut changed = true;
    while changed {
        changed = false;
        any_change |= Inliner::new(
            function,
            local_to_group,
            upvalue_to_group,
            &mut local_usages,
        )
        .inline_rvalues();

        // remove unused locals
        for block in function.blocks_mut() {
            for stat_index in 0..block.len() {
                if let ast::Statement::Assign(assign) = &block[stat_index]
                    && assign.left.len() == 1
                    && assign.right.len() == 1
                    && let ast::LValue::Local(local) = &assign.left[0]
                {
                    let rvalue = &assign.right[0];
                    let has_side_effects = rvalue.has_side_effects();
                    // TODO: REFACTOR: is_some_and
                    if !upvalue_to_group.contains_key(local)
                        && local_usages.get(local).map_or(true, |&u| u == 0)
                    {
                        if has_side_effects {
                            // TODO: PERF: dont clone
                            let new_stat = match rvalue {
                                ast::RValue::Call(call)
                                | ast::RValue::Select(ast::Select::Call(call)) => {
                                    Some(call.clone().into())
                                }
                                ast::RValue::MethodCall(method_call)
                                | ast::RValue::Select(ast::Select::MethodCall(method_call)) => {
                                    Some(method_call.clone().into())
                                }
                                _ => None,
                            };
                            if let Some(new_stat) = new_stat {
                                block[stat_index] = new_stat;
                                changed = true;
                            }
                        } else {
                            block[stat_index] = ast::Empty {}.into();
                            changed = true;
                        }
                    }
                }
            }
        }

        for block in function.blocks_mut() {
            // we check block.ast.len() elsewhere and do `i - ` here and elsewhere so we need to get rid of empty statements
            // TODO: fix ^
            block.retain(|s| s.as_empty().is_none());

            // `t = {} t.a = 1` -> `t = { a = 1 }`
            changed |= fold_table_fields(block, &effect_observable_locals) != 0;
            changed |= fold_set_lists(block, &mut local_usages) != 0;
        }
        any_change |= changed;
    }
    // we check block.ast.len() elsewhere and do `i - ` here and elsewhere so we need to get rid of empty statements
    // TODO: fix ^
    for block in function.blocks_mut() {
        block.retain(|s| s.as_empty().is_none());
    }
    any_change
}

#[cfg(test)]
mod tests {
    use ast::{
        Assign, Block, Call, Closure, Global, Index, LValue, Literal, Local, RValue, RcLocal,
        SetList, Table, Upvalue,
    };

    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{fold_set_lists, fold_table_fields};

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn field_assignment(object: &RcLocal, key: RValue, value: RValue) -> ast::Statement {
        Assign::new(
            vec![LValue::Index(Index::new(object.clone().into(), key))],
            vec![value],
        )
        .into()
    }

    fn closure(upvalues: Vec<Upvalue>) -> RValue {
        Closure {
            function: Default::default(),
            upvalues,
        }
        .into()
    }

    #[test]
    fn folds_adjacent_callback_field_without_target_capture() {
        let object = local("callbacks");
        let mut block = Block(vec![
            Assign::new(vec![object.clone().into()], vec![Table::default().into()]).into(),
            field_assignment(
                &object,
                Literal::String(b"ready".to_vec()).into(),
                closure(Vec::new()),
            ),
        ]);

        assert_eq!(fold_table_fields(&mut block, &FxHashSet::default()), 1);
        assert_eq!(block.len(), 1);
        assert_eq!(
            block[0].as_assign().unwrap().right[0]
                .as_table()
                .unwrap()
                .0
                .len(),
            1
        );
    }

    #[test]
    fn keeps_callback_field_that_captures_target_table() {
        let object = local("callbacks");
        let mut block = Block(vec![
            Assign::new(vec![object.clone().into()], vec![Table::default().into()]).into(),
            field_assignment(
                &object,
                Literal::String(b"ready".to_vec()).into(),
                closure(vec![Upvalue::Ref(object.clone())]),
            ),
        ]);

        assert_eq!(fold_table_fields(&mut block, &FxHashSet::default()), 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_field_after_open_table_call_tail() {
        let object = local("values");
        let call = Call::new(RValue::from(Global::from("produce")), Vec::new());
        let mut block = Block(vec![
            Assign::new(
                vec![object.clone().into()],
                vec![Table(vec![(None, call.into())]).into()],
            )
            .into(),
            field_assignment(
                &object,
                Literal::String(b"status".to_vec()).into(),
                Literal::Boolean(true).into(),
            ),
        ]);

        assert_eq!(fold_table_fields(&mut block, &FxHashSet::default()), 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_effectful_field_value_after_table_assignment() {
        let object = local("state");
        let observe = Call::new(RValue::from(Global::from("observe")), Vec::new());
        let observable = FxHashSet::from_iter([object.clone()]);
        let mut block = Block(vec![
            Assign::new(vec![object.clone().into()], vec![Table::default().into()]).into(),
            field_assignment(
                &object,
                Literal::String(b"value".to_vec()).into(),
                observe.into(),
            ),
        ]);

        assert_eq!(fold_table_fields(&mut block, &observable), 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn folds_effectful_field_value_into_fresh_local_table() {
        let object = local("state");
        let observe = Call::new(RValue::from(Global::from("observe")), Vec::new());
        let mut declaration =
            Assign::new(vec![object.clone().into()], vec![Table::default().into()]);
        declaration.prefix = true;
        let mut block = Block(vec![
            declaration.into(),
            field_assignment(
                &object,
                Literal::String(b"value".to_vec()).into(),
                observe.into(),
            ),
        ]);

        assert_eq!(fold_table_fields(&mut block, &FxHashSet::default()), 1);
        assert_eq!(block.len(), 1);
    }

    #[test]
    fn folds_set_list_through_adjacent_table_alias() {
        let table = local("table");
        let alias = local("alias");
        let mut block = Block(vec![
            Assign::new(vec![table.clone().into()], vec![Table::default().into()]).into(),
            Assign::new(vec![alias.clone().into()], vec![table.into()]).into(),
            SetList::new(
                alias.clone(),
                1,
                vec![Literal::String(b"value".to_vec()).into()],
                None,
            )
            .into(),
        ]);
        let mut local_usages = FxHashMap::from_iter([(alias.clone(), 1)]);

        assert_eq!(fold_set_lists(&mut block, &mut local_usages), 1);
        assert_eq!(
            block[0].as_assign().unwrap().right[0].as_table().unwrap().0,
            vec![(None, Literal::String(b"value".to_vec()).into())]
        );
        assert!(block[2].as_empty().is_some());
        assert_eq!(local_usages[&alias], 0);
    }

    #[test]
    fn moves_unobserved_table_construction_past_value_preparation() {
        let table = local("table");
        let value = local("value");
        let mut block = Block(vec![
            Assign::new(vec![table.clone().into()], vec![Table::default().into()]).into(),
            Assign::new(
                vec![value.clone().into()],
                vec![Literal::String(b"value".to_vec()).into()],
            )
            .into(),
            SetList::new(table.clone(), 1, vec![value.clone().into()], None).into(),
        ]);
        let mut local_usages = FxHashMap::from_iter([(table.clone(), 1), (value, 1)]);

        assert_eq!(fold_set_lists(&mut block, &mut local_usages), 1);
        assert!(block[0].as_empty().is_some());
        let moved = block[2].as_assign().unwrap();
        assert_eq!(moved.left, vec![table.into()]);
        assert_eq!(moved.right[0].as_table().unwrap().0.len(), 1);
    }

    #[test]
    fn keeps_constructor_local_read_before_intervening_call() {
        let table = local("table");
        let source = local("source");
        let mut block = Block(vec![
            Assign::new(
                vec![table.clone().into()],
                vec![
                    Table(vec![(
                        Some(Literal::String(b"tag".to_vec()).into()),
                        source.clone().into(),
                    )])
                    .into(),
                ],
            )
            .into(),
            Call::new(Global::from("mutate").into(), Vec::new()).into(),
            SetList::new(
                table.clone(),
                1,
                vec![Literal::String(b"value".to_vec()).into()],
                None,
            )
            .into(),
        ]);
        let mut local_usages = FxHashMap::from_iter([(table, 1), (source, 1)]);

        assert_eq!(fold_set_lists(&mut block, &mut local_usages), 0);
        assert!(block[2].as_set_list().is_some());
    }
}
