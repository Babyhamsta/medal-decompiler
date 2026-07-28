mod deserializer;
mod error;
mod instruction;
mod lifter;
mod op_code;

#[cfg(test)]
mod compatibility_tests;

use ast::{
    Traverse, local_declarations::LocalDeclarer, name_locals::name_locals,
    replace_locals::replace_locals,
};

use by_address::ByAddress;
use cfg::{
    function::Function,
    ssa::{
        self,
        structuring::{structure_conditionals, structure_jumps},
    },
};
use indexmap::IndexMap;

use error::catch_phase;
pub use error::{DecompileError, DecompilePhase};
use lifter::Lifter;

//use cfg_ir::{dot, function::Function, ssa};
use clap::Parser;
use parking_lot::Mutex;
use petgraph::algo::dominators::simple_fast;

use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;

use deserializer::bytecode::Bytecode;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    paths: Vec<String>,
    /// Number of threads to use (0 = automatic)
    #[clap(short, long, default_value_t = 0)]
    threads: usize,
    /// op = op * key % 256
    /// For Roblox client bytecode, use 203
    #[clap(short, long, default_value_t = 1)]
    key: u8,
    #[clap(short, long)]
    recursive: bool,
    #[clap(short, long)]
    verbose: bool,
}

pub fn decompile_bytecode(bytecode: &[u8], encode_key: u8) -> Result<String, DecompileError> {
    try_decompile_bytecode(bytecode, encode_key)
}

pub fn try_decompile_bytecode(bytecode: &[u8], encode_key: u8) -> Result<String, DecompileError> {
    catch_phase(DecompilePhase::Unknown, None, None, || {
        try_decompile_bytecode_inner(bytecode, encode_key)
    })?
}

fn try_decompile_bytecode_inner(bytecode: &[u8], encode_key: u8) -> Result<String, DecompileError> {
    let parsed = deserializer::deserialize(bytecode, encode_key).map_err(|detail| {
        DecompileError::new(
            DecompilePhase::Deserialize,
            None,
            None,
            "valid Luau bytecode",
            detail,
        )
    })?;
    let Bytecode::Chunk(chunk) = parsed else {
        let Bytecode::Error(message) = parsed else {
            unreachable!()
        };
        return Err(DecompileError::new(
            DecompilePhase::Deserialize,
            None,
            None,
            "valid Luau bytecode",
            message,
        ));
    };

    decompile_chunk(chunk)
}

fn decompile_chunk(chunk: deserializer::chunk::Chunk) -> Result<String, DecompileError> {
    let mut lifted = Vec::new();
    let mut stack = vec![(Arc::<Mutex<ast::Function>>::default(), chunk.main)];
    while let Some((ast_func, func_id)) = stack.pop() {
        let (function, upvalues, child_functions) =
            catch_phase(DecompilePhase::Lift, Some(func_id), None, || {
                Lifter::lift(&chunk.functions, &chunk.string_table, func_id)
            })?;
        lifted.push((ast_func, function, upvalues));
        stack.extend(child_functions.into_iter().map(|(a, f)| (a.0, f)));
    }

    let (main, ..) = lifted.first().unwrap().clone();
    let mut upvalues = lifted
        .into_iter()
        .map(|(ast_function, function, upvalues_in)| {
            decompile_function(ast_function, function, upvalues_in)
        })
        .collect::<Result<FxHashMap<_, _>, DecompileError>>()?;

    let main = ByAddress(main);
    upvalues.remove(&main);
    let mut body = catch_phase(DecompilePhase::Link, None, None, || {
        let mut body = Arc::try_unwrap(main.0).unwrap().into_inner().body;
        link_upvalues(&mut body, &mut upvalues);
        body
    })?;
    if block_contains_unsupported_jump(&mut body) {
        return Err(DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "structured control flow",
            "control-flow structuring left unsupported goto or label nodes",
        ));
    }
    catch_phase(DecompilePhase::Format, None, None, || {
        ast::recover_function_syntax(&mut body);
        name_locals(&mut body, false);
        body.to_string()
    })
}

fn block_contains_unsupported_jump(block: &mut ast::Block) -> bool {
    for statement in &mut block.0 {
        if matches!(
            statement,
            ast::Statement::Goto(_) | ast::Statement::Label(_)
        ) {
            return true;
        }

        let nested_jump = match statement {
            ast::Statement::If(r#if) => {
                block_contains_unsupported_jump(&mut r#if.then_block.lock())
                    || block_contains_unsupported_jump(&mut r#if.else_block.lock())
            }
            ast::Statement::While(r#while) => {
                block_contains_unsupported_jump(&mut r#while.block.lock())
            }
            ast::Statement::Repeat(repeat) => {
                block_contains_unsupported_jump(&mut repeat.block.lock())
            }
            ast::Statement::NumericFor(numeric_for) => {
                block_contains_unsupported_jump(&mut numeric_for.block.lock())
            }
            ast::Statement::GenericFor(generic_for) => {
                block_contains_unsupported_jump(&mut generic_for.block.lock())
            }
            _ => false,
        };
        if nested_jump {
            return true;
        }

        let mut closure_jump = false;
        statement.traverse_rvalues(&mut |rvalue| {
            if !closure_jump && let ast::RValue::Closure(closure) = rvalue {
                closure_jump = block_contains_unsupported_jump(&mut closure.function.lock().body);
            }
        });
        if closure_jump {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod output_tests {
    use super::block_contains_unsupported_jump;

    #[test]
    fn rejects_unsupported_jump_nodes() {
        let label = ast::Label("exit".to_owned());
        let mut body = ast::Block(vec![ast::Goto::new(label).into()]);

        assert!(block_contains_unsupported_jump(&mut body));
    }
}

#[cfg(test)]
mod structured_error_tests {
    use super::{DecompileError, DecompilePhase, catch_phase, decompile_bytecode};

    #[test]
    fn structured_error_display_includes_available_context() {
        let error = DecompileError::new(
            DecompilePhase::Declaration,
            Some(7),
            Some(12),
            "stable binding identity",
            "local escaped as a global",
        );

        assert_eq!(
            error.to_string(),
            "[declaration] function=7 instruction=12 \
             invariant=stable binding identity: local escaped as a global"
        );
    }

    #[test]
    fn phase_boundary_converts_panic_without_emitting_source() {
        let error = catch_phase(DecompilePhase::Ssa, Some(7), Some(12), || {
            panic!("bad merge")
        })
        .unwrap_err();

        assert_eq!(error.phase, DecompilePhase::Ssa);
        assert_eq!(error.function_id, Some(7));
        assert_eq!(error.instruction, Some(12));
        assert_eq!(error.invariant, "panic-free decompilation");
        assert_eq!(error.detail, "bad merge");
    }

    #[test]
    fn invalid_bytecode_returns_deserialize_error() {
        let error = decompile_bytecode(&[0xff], 1).unwrap_err();

        assert_eq!(error.phase, DecompilePhase::Deserialize);
        assert_eq!(error.invariant, "valid Luau bytecode");
        assert!(!error.to_string().starts_with("--"));
    }
}

fn decompile_function(
    ast_function: Arc<Mutex<ast::Function>>,
    mut function: Function,
    upvalues_in: Vec<ast::RcLocal>,
) -> Result<(ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>), DecompileError> {
    let function_id = function.id;
    let (local_count, upvalue_to_group, local_to_group) =
        catch_phase(DecompilePhase::Ssa, Some(function_id), None, || {
            let (local_count, local_groups, upvalue_in_groups, upvalue_passed_groups) =
                cfg::ssa::construct(&mut function, &upvalues_in);
            let upvalue_passed_groups = upvalue_passed_groups
                .into_iter()
                .map(|members| {
                    let source = members
                        .iter()
                        .next()
                        .cloned()
                        .expect("upvalue group must contain a source local");
                    (function.new_synthetic_local(&source), members)
                })
                .collect::<Vec<_>>();
            let upvalue_to_group = upvalue_in_groups
                .into_iter()
                .chain(upvalue_passed_groups)
                .flat_map(|(i, g)| g.into_iter().map(move |u| (u, i.clone())))
                .collect::<IndexMap<_, _>>();
            // TODO: do we even need this?
            let local_to_group = local_groups
                .into_iter()
                .enumerate()
                .flat_map(|(i, g)| g.into_iter().map(move |l| (l, i)))
                .collect::<FxHashMap<_, _>>();
            (local_count, upvalue_to_group, local_to_group)
        })?;

    // TODO: REFACTOR: some way to write a macro that states
    // if cfg::ssa::inline results in change then structure_jumps, structure_compound_conditionals,
    // structure_for_loops and remove_unnecessary_params must run again.
    // if structure_compound_conditionals results in change then dominators and post dominators
    // must be recalculated.
    // etc.
    // the macro could also maybe generate an optimal ordering?
    catch_phase(DecompilePhase::Structure, Some(function_id), None, || {
        let mut changed = true;
        while changed {
            changed = false;

            let dominators = simple_fast(function.graph(), function.entry().unwrap());
            changed |= structure_jumps(&mut function, &dominators);

            ssa::inline::inline(&mut function, &local_to_group, &upvalue_to_group);

            if structure_conditionals(&mut function)
            // || {
            //     let post_dominators = post_dominators(function.graph_mut());
            //     structure_for_loops(&mut function, &dominators, &post_dominators)
            // }
            // we can't structure method calls like this because of __namecall
            // || structure_method_calls(&mut function)
            {
                changed = true;
            }
            let mut local_map = FxHashMap::default();
            // TODO: loop until returns false?
            if ssa::construct::remove_unnecessary_params(&mut function, &mut local_map) {
                changed = true;
            }
            ssa::construct::apply_local_map(&mut function, local_map);
        }
    })?;

    catch_phase(
        DecompilePhase::SsaDestruction,
        Some(function_id),
        None,
        || {
            ssa::Destructor::new(
                &mut function,
                upvalue_to_group,
                upvalues_in.iter().cloned().collect(),
                local_count,
            )
            .destruct();
        },
    )?;

    let (params, is_variadic, mut block) =
        catch_phase(DecompilePhase::Restructure, Some(function_id), None, || {
            let params = std::mem::take(&mut function.parameters);
            let is_variadic = function.is_variadic;
            let block: ast::Block = restructure::lift(function).into();
            (params, is_variadic, block)
        })?;

    catch_phase(DecompilePhase::AstRecovery, Some(function_id), None, || {
        ast::eliminate_aliases_with_protected(&mut block, &upvalues_in);
        ast::recover_expressions_with_protected(&mut block, &upvalues_in);
        ast::cleanup_control_flow(&mut block);
    })?;

    let block = Arc::new(Mutex::new(block));
    catch_phase(DecompilePhase::Declaration, Some(function_id), None, || {
        LocalDeclarer::default().declare_locals(
            // TODO: why does block.clone() not work?
            Arc::clone(&block),
            &upvalues_in.iter().chain(params.iter()).cloned().collect(),
        );
    })?;

    catch_phase(DecompilePhase::AstRecovery, Some(function_id), None, || {
        let mut ast_function = ast_function.lock();
        ast_function.body = Arc::try_unwrap(block).unwrap().into_inner();
        ast_function.parameters = params;
        ast_function.is_variadic = is_variadic;
    })?;

    Ok((ByAddress(ast_function), upvalues_in))
}

fn link_upvalues(
    body: &mut ast::Block,
    upvalues: &mut FxHashMap<ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>>,
) {
    let mut promoted_names = FxHashMap::<ast::RcLocal, FxHashSet<String>>::default();
    link_upvalues_in_scope(body, upvalues, &mut promoted_names);
    for (target, names) in promoted_names {
        if names.len() == 1 {
            let mut target = target.0.0.lock();
            if target.0.is_none() {
                target.0 = names.into_iter().next();
            }
        }
    }
}

fn link_upvalues_in_scope(
    body: &mut ast::Block,
    upvalues: &mut FxHashMap<ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>>,
    promoted_names: &mut FxHashMap<ast::RcLocal, FxHashSet<String>>,
) {
    for stat in &mut body.0 {
        stat.traverse_rvalues(&mut |rvalue| {
            if let ast::RValue::Closure(closure) = rvalue {
                let old_upvalues = upvalues[&closure.function].clone();
                let mut function = closure.function.lock();
                // TODO: inefficient, try constructing a map of all up -> new up first
                // and then call replace_locals on main body
                let mut local_map =
                    FxHashMap::with_capacity_and_hasher(old_upvalues.len(), Default::default());
                for (old, new) in
                    old_upvalues
                        .iter()
                        .zip(closure.upvalues.iter().map(|u| match u {
                            ast::Upvalue::Copy(l) | ast::Upvalue::Ref(l) => l,
                        }))
                {
                    // println!("{} -> {}", old, new);
                    local_map.insert(old.clone(), new.clone());
                }
                link_upvalues(&mut function.body, upvalues);
                for (old, new) in
                    old_upvalues
                        .iter()
                        .zip(closure.upvalues.iter().map(|u| match u {
                            ast::Upvalue::Copy(l) | ast::Upvalue::Ref(l) => l,
                        }))
                {
                    let debug_name = old.0.0.lock().0.clone();
                    if let Some(name) =
                        debug_name.filter(|name| ast::is_valid_identifier(name.as_bytes()))
                        && new.0.0.lock().0.is_none()
                    {
                        promoted_names.entry(new.clone()).or_default().insert(name);
                    }
                }
                replace_locals(&mut function.body, &local_map);
            }
        });
        match stat {
            ast::Statement::If(r#if) => {
                link_upvalues_in_scope(&mut r#if.then_block.lock(), upvalues, promoted_names);
                link_upvalues_in_scope(&mut r#if.else_block.lock(), upvalues, promoted_names);
            }
            ast::Statement::While(r#while) => {
                link_upvalues_in_scope(&mut r#while.block.lock(), upvalues, promoted_names);
            }
            ast::Statement::Repeat(repeat) => {
                link_upvalues_in_scope(&mut repeat.block.lock(), upvalues, promoted_names);
            }
            ast::Statement::NumericFor(numeric_for) => {
                link_upvalues_in_scope(&mut numeric_for.block.lock(), upvalues, promoted_names);
            }
            ast::Statement::GenericFor(generic_for) => {
                link_upvalues_in_scope(&mut generic_for.block.lock(), upvalues, promoted_names);
            }
            _ => {}
        }
    }
}
