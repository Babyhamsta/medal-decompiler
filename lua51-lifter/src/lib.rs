use std::cell::RefCell;

use ast::{
    Traverse, local_declarations::LocalDeclarer, name_locals::name_locals,
    replace_locals::replace_locals,
};
use by_address::ByAddress;
use cfg::{
    function::Function,
    ssa::{
        self,
        structuring::{structure_conditionals_lua51, structure_jumps, structure_method_calls},
    },
};
use error::catch_phase;
use indexmap::IndexMap;
use parking_lot::Mutex;
use petgraph::algo::dominators::simple_fast;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;

use lifter::Lifter;
use lua51_deserializer::chunk::Chunk;

pub use error::{DecompileError, DecompilePhase};

mod disasm;
mod error;
mod lifter;
mod validate;

pub use disasm::{DisassembleError, ProtoSelection, disassemble, list_prototypes};

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[cfg(all(
    feature = "mimalloc",
    not(feature = "dhat-heap"),
    not(target_arch = "wasm32")
))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub fn decompile_bytecode(bytecode: &[u8]) -> Result<String, DecompileError> {
    try_decompile_bytecode(bytecode)
}

pub fn try_decompile_bytecode(bytecode: &[u8]) -> Result<String, DecompileError> {
    catch_phase(DecompilePhase::Unknown, None, None, || {
        try_decompile_bytecode_inner(bytecode)
    })?
}

fn try_decompile_bytecode_inner(bytecode: &[u8]) -> Result<String, DecompileError> {
    let parsed = catch_phase(DecompilePhase::Deserialize, None, None, || {
        lua51_deserializer::deserialize(bytecode)
    })?;
    let chunk = parsed.map_err(|error| {
        DecompileError::new(
            DecompilePhase::Deserialize,
            None,
            None,
            "valid Lua 5.1 bytecode",
            error.to_string(),
        )
    })?;
    let plan = validate::function_tree(&chunk.function, bytecode.len())?;
    decompile_chunk(chunk, plan)
}

fn decompile_chunk(
    chunk: Chunk<'_>,
    plan: validate::ValidationPlan,
) -> Result<String, DecompileError> {
    let root_is_dumped_function = chunk.function.line_defined != 0
        || chunk.function.last_line_defined != 0
        || chunk.function.number_of_parameters != 0
        || chunk.function.number_of_upvalues != 0
        || chunk.function.vararg_flag & (1 | 4) != 0;
    let mut lifted = Vec::new();
    lifted
        .try_reserve_exact(plan.instances)
        .map_err(|error| resource_error(DecompilePhase::Lift, None, error))?;
    let mut next_function_id = 0usize;
    let (function, upvalues, has_legacy_arg) =
        catch_phase(DecompilePhase::Lift, Some(0), None, || {
            Lifter::lift(&chunk.function, &mut lifted, &mut next_function_id)
        })?;
    if next_function_id != plan.instances {
        return Err(DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "validated prototype expansion",
            format!(
                "planned {} instances/{} instructions but lifted {next_function_id} instances",
                plan.instances, plan.instructions
            ),
        ));
    }
    let main = Arc::<Mutex<ast::Function>>::default();
    lifted.push((main.clone(), function, upvalues, has_legacy_arg));
    lifted.reverse();
    drop(chunk);

    let recovered = lifted
        .into_par_iter()
        .map(|(ast_function, function, upvalues_in, has_legacy_arg)| {
            let declared_parameters = u8::try_from(
                function
                    .parameters
                    .len()
                    .saturating_sub(usize::from(has_legacy_arg)),
            )
            .unwrap_or(u8::MAX);
            let declared_variadic = function.is_variadic;
            let upvalues_out = upvalues_in.clone();
            match decompile_function(
                ast_function.clone(),
                function,
                upvalues_in,
                has_legacy_arg,
            ) {
                Ok(decompiled) => decompiled,
                Err(error) => {
                    stub_unrecovered_function(
                        &ast_function,
                        &error,
                        declared_parameters,
                        declared_variadic,
                    );
                    (ByAddress(ast_function), upvalues_out)
                }
            }
        });
    let mut results = Vec::new();
    results
        .try_reserve_exact(plan.instances)
        .map_err(|error| resource_error(DecompilePhase::AstRecovery, None, error))?;
    recovered.collect_into_vec(&mut results);
    let mut decompiled_upvalues = FxHashMap::default();
    decompiled_upvalues
        .try_reserve(results.len())
        .map_err(|error| resource_error(DecompilePhase::Link, None, error))?;
    decompiled_upvalues.extend(results);

    let main = ByAddress(main);
    let main_upvalue_list = decompiled_upvalues.remove(&main).unwrap_or_default();
    let main_upvalues = main_upvalue_list.iter().cloned().collect::<FxHashSet<_>>();
    let mut body = catch_phase(DecompilePhase::Link, None, None, || {
        let mut root = Arc::try_unwrap(main.0).unwrap().into_inner();
        link_upvalues(&mut root.body, &mut decompiled_upvalues);
        if root_is_dumped_function {
            let closure = ast::Closure {
                function: ByAddress(Arc::new(Mutex::new(root))),
                upvalues: main_upvalue_list
                    .iter()
                    .cloned()
                    .map(ast::Upvalue::Ref)
                    .collect(),
            };
            let mut statements = Vec::with_capacity(2);
            if !main_upvalue_list.is_empty() {
                let mut declaration = ast::Assign::new(
                    main_upvalue_list
                        .iter()
                        .cloned()
                        .map(ast::LValue::Local)
                        .collect(),
                    main_upvalue_list
                        .iter()
                        .map(|_| ast::RValue::Literal(ast::Literal::Nil))
                        .collect(),
                );
                declaration.prefix = true;
                statements.push(ast::Statement::Assign(declaration));
            }
            statements.push(ast::Statement::Return(ast::Return::new(vec![closure.into()])));
            ast::Block(statements)
        } else {
            root.body
        }
    })?;
    drop(decompiled_upvalues);
    let initially_visible = if root_is_dumped_function {
        FxHashSet::default()
    } else {
        main_upvalues
    };

    catch_phase(DecompilePhase::AstRecovery, None, None, || {
        ast::lower_residual_set_lists(&mut body);
    })?;
    if let Some(kind) = unsupported_node_kind(&mut body) {
        return Err(DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "Lua 5.1 source-level AST",
            format!("reconstruction left an unsupported {kind} node"),
        ));
    }

    catch_phase(DecompilePhase::Format, None, None, || {
        ast::recover_function_syntax(&mut body);
        ast::propagate_parameter_names(&mut body);
        name_locals(&mut body, false);
        ast::validate_bindings(&body, &initially_visible)?;
        ast::narrow_local_scopes(&mut body);
        ast::combine_local_declarations(&mut body);
        ast::validate_bindings(&body, &initially_visible)?;
        Ok(ast::format_lua51(&body))
    })?
    .map_err(|error: ast::BindingResolutionError| {
        DecompileError::new(
            DecompilePhase::Format,
            None,
            None,
            "every local reference resolves to its lexical binding",
            error.to_string(),
        )
    })
}

fn decompile_function(
    ast_function: Arc<Mutex<ast::Function>>,
    mut function: Function,
    upvalues_in: Vec<ast::RcLocal>,
    has_legacy_arg: bool,
) -> Result<(ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>), DecompileError> {
    let function_id = function.id;
    let ssa_result = catch_phase(
        DecompilePhase::Ssa,
        Some(function_id),
        None,
        || -> Result<_, cfg::ssa::SsaError> {
            let (local_count, local_groups, upvalue_in_groups, upvalue_passed_groups) =
                cfg::ssa::construct(&mut function, &upvalues_in)?;
            function
                .validate_reference_bindings()
                .expect("SSA must preserve reference binding classes");
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
                .flat_map(|(source, group)| {
                    group
                        .into_iter()
                        .map(move |upvalue| (upvalue, source.clone()))
                })
                .collect::<IndexMap<_, _>>();
            let local_to_group = local_groups
                .into_iter()
                .enumerate()
                .flat_map(|(group, locals)| locals.into_iter().map(move |local| (local, group)))
                .collect::<FxHashMap<_, _>>();
            Ok((local_count, upvalue_to_group, local_to_group))
        },
    )?;
    let (local_count, upvalue_to_group, local_to_group) =
        ssa_result.map_err(|error| ssa_error(function_id, error))?;
    let upvalue_to_group = RefCell::new(upvalue_to_group);

    let recovery_report = catch_phase(DecompilePhase::Structure, Some(function_id), None, || {
        let mut scheduler = cfg::recovery::PassScheduler::new(32);
        scheduler.add_pass("structure-jumps", |function| {
            let dominators = simple_fast(function.graph(), function.entry().unwrap());
            if structure_jumps(function, &dominators) {
                cfg::recovery::PassChange::cfg().union(cfg::recovery::PassChange::ast())
            } else {
                cfg::recovery::PassChange::none()
            }
        });
        scheduler.add_pass("inline", |function| {
            #[cfg(feature = "verify-inline-change")]
            let before = cfg::recovery::structural_fingerprint(function);
            let changed =
                ssa::inline::inline(function, &local_to_group, &upvalue_to_group.borrow());
            #[cfg(feature = "verify-inline-change")]
            assert_eq!(
                changed,
                before != cfg::recovery::structural_fingerprint(function),
                "inline change flag disagrees with structural fingerprint"
            );
            if changed {
                cfg::recovery::PassChange::dataflow().union(cfg::recovery::PassChange::ast())
            } else {
                cfg::recovery::PassChange::none()
            }
        });
        scheduler.add_pass("structure-conditionals", |function| {
            if structure_conditionals_lua51(function) {
                cfg::recovery::PassChange::cfg()
                    .union(cfg::recovery::PassChange::dataflow())
                    .union(cfg::recovery::PassChange::ast())
            } else {
                cfg::recovery::PassChange::none()
            }
        });
        scheduler.add_pass("structure-method-calls", |function| {
            if structure_method_calls(function) {
                cfg::recovery::PassChange::dataflow().union(cfg::recovery::PassChange::ast())
            } else {
                cfg::recovery::PassChange::none()
            }
        });
        scheduler.add_pass("remove-unnecessary-params", |function| {
            let mut local_map = FxHashMap::default();
            if ssa::construct::remove_unnecessary_params(function, &mut local_map) {
                ssa::construct::apply_local_map_to_upvalue_groups(
                    &mut upvalue_to_group.borrow_mut(),
                    &local_map,
                );
                ssa::construct::apply_local_map(function, local_map);
                cfg::recovery::PassChange::cfg()
                    .union(cfg::recovery::PassChange::dataflow())
                    .union(cfg::recovery::PassChange::ast())
            } else {
                cfg::recovery::PassChange::none()
            }
        });
        let report = scheduler.run(&mut function)?;
        function
            .validate_reference_bindings()
            .expect("structuring must preserve reference binding classes");
        Ok::<_, cfg::recovery::SchedulerError>(report)
    })?
    .map_err(|error| {
        DecompileError::new(
            DecompilePhase::Structure,
            Some(function_id),
            None,
            "deterministic reconstruction",
            error.to_string(),
        )
    })?;
    let recovery_facts = recovery_report.facts;

    catch_phase(
        DecompilePhase::SsaDestruction,
        Some(function_id),
        None,
        || {
            ssa::Destructor::new(
                &mut function,
                upvalue_to_group.into_inner(),
                upvalues_in.iter().cloned().collect(),
                local_count,
            )
            .destruct();
        },
    )?;
    debug_assert_eq!(recovery_facts.function_id(), function_id);
    let (parameters, implicit_parameters, is_variadic, mut block) =
        catch_phase(DecompilePhase::Restructure, Some(function_id), None, || {
            let mut parameters = std::mem::take(&mut function.parameters);
            let implicit_parameters = if has_legacy_arg {
                let argument = parameters
                    .pop()
                    .expect("validated legacy vararg function must have an arg register");
                argument.0.0.lock().0 = Some("arg".to_owned());
                vec![argument]
            } else {
                Vec::new()
            };
            let is_variadic = function.is_variadic;
            let block: ast::Block = restructure::lift(function, &recovery_facts).into();
            (parameters, implicit_parameters, is_variadic, block)
        })?;

    catch_phase(DecompilePhase::AstRecovery, Some(function_id), None, || {
        const RECOVERY_ROUNDS: usize = 4;
        let unfoldable = upvalues_in
            .iter()
            .chain(parameters.iter())
            .chain(implicit_parameters.iter())
            .cloned()
            .collect::<Vec<_>>();
        for _ in 0..RECOVERY_ROUNDS {
            let mut changes = ast::eliminate_aliases_with_protected(&mut block, &upvalues_in);
            changes += ast::fold_table_slots(&mut block, &unfoldable);
            let recovered = ast::recover_expressions_lua51(&mut block, &upvalues_in);
            changes += recovered.short_circuits + recovered.inlined_temporaries;
            if changes == 0 {
                break;
            }
        }
        ast::cleanup_control_flow_lua51(&mut block);
    })?;

    let block = Arc::new(Mutex::new(block));
    catch_phase(DecompilePhase::Declaration, Some(function_id), None, || {
        let initially_visible = upvalues_in
            .iter()
            .chain(parameters.iter())
            .chain(implicit_parameters.iter())
            .cloned()
            .collect();
        LocalDeclarer::default().declare_locals(Arc::clone(&block), &initially_visible);
        ast::lower_lua51_continues(&mut block.lock());
        ast::validate_bindings(&block.lock(), &initially_visible)
    })?
    .map_err(|error| {
        DecompileError::new(
            DecompilePhase::Declaration,
            Some(function_id),
            None,
            "every local reference resolves to its lexical binding",
            error.to_string(),
        )
    })?;

    catch_phase(DecompilePhase::AstRecovery, Some(function_id), None, || {
        let mut target = ast_function.lock();
        target.body = Arc::try_unwrap(block).unwrap().into_inner();
        target.parameters = parameters;
        target.implicit_parameters = implicit_parameters;
        target.is_variadic = is_variadic;
    })?;
    Ok((ByAddress(ast_function), upvalues_in))
}

fn ssa_error(function_id: usize, error: cfg::ssa::SsaError) -> DecompileError {
    DecompileError::new(
        DecompilePhase::Ssa,
        Some(function_id),
        None,
        "bounded SSA analysis",
        error.to_string(),
    )
}

fn resource_error(
    phase: DecompilePhase,
    function_id: Option<usize>,
    error: impl std::fmt::Display,
) -> DecompileError {
    DecompileError::new(
        phase,
        function_id,
        None,
        "bounded reconstruction allocation",
        error.to_string(),
    )
}

fn stub_unrecovered_function(
    ast_function: &Arc<Mutex<ast::Function>>,
    error: &DecompileError,
    declared_parameters: u8,
    declared_variadic: bool,
) {
    let reason = error.to_string();
    let mut function = ast_function.lock();
    function.parameters = (0..declared_parameters)
        .map(|_| ast::RcLocal::default())
        .collect();
    function.is_variadic = declared_variadic;
    function.body = ast::Block(vec![
        ast::Comment::new(format!("decompilation failed: {reason}")).into(),
        ast::Call::new(
            ast::Global::new(b"error".to_vec()).into(),
            vec![
                ast::Literal::String(format!("unrecovered function: {reason}").into_bytes()).into(),
            ],
        )
        .into(),
    ]);
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
    for statement in &mut body.0 {
        statement.traverse_rvalues(&mut |rvalue| {
            if let ast::RValue::Closure(closure) = rvalue {
                let old_upvalues = upvalues[&closure.function].clone();
                let mut function = closure.function.lock();
                let mut local_map =
                    FxHashMap::with_capacity_and_hasher(old_upvalues.len(), Default::default());
                for (old, new) in old_upvalues
                    .iter()
                    .zip(closure.upvalues.iter().map(|upvalue| match upvalue {
                        ast::Upvalue::Copy(local) | ast::Upvalue::Ref(local) => local,
                    }))
                {
                    local_map.insert(old.clone(), new.clone());
                    let debug_name = old.0.0.lock().0.clone();
                    if let Some(name) =
                        debug_name.filter(|name| ast::is_valid_identifier(name.as_bytes()))
                        && new.0.0.lock().0.is_none()
                    {
                        promoted_names.entry(new.clone()).or_default().insert(name);
                    }
                }
                link_upvalues(&mut function.body, upvalues);
                replace_locals(&mut function.body, &local_map);
            }
        });
        match statement {
            ast::Statement::If(value) => {
                link_upvalues_in_scope(&mut value.then_block.lock(), upvalues, promoted_names);
                link_upvalues_in_scope(&mut value.else_block.lock(), upvalues, promoted_names);
            }
            ast::Statement::While(value) => {
                link_upvalues_in_scope(&mut value.block.lock(), upvalues, promoted_names);
            }
            ast::Statement::Repeat(value) => {
                link_upvalues_in_scope(&mut value.block.lock(), upvalues, promoted_names);
            }
            ast::Statement::NumericFor(value) => {
                link_upvalues_in_scope(&mut value.block.lock(), upvalues, promoted_names);
            }
            ast::Statement::GenericFor(value) => {
                link_upvalues_in_scope(&mut value.block.lock(), upvalues, promoted_names);
            }
            _ => {}
        }
    }
}

fn unsupported_node_kind(block: &mut ast::Block) -> Option<&'static str> {
    for statement in &mut block.0 {
        match statement {
            ast::Statement::Goto(_) => return Some("goto"),
            ast::Statement::Label(_) => return Some("label"),
            ast::Statement::SetList(_) => return Some("set-list"),
            ast::Statement::Continue(_) => return Some("continue"),
            ast::Statement::Class(_) => return Some("class"),
            _ => {}
        }
        let nested = match statement {
            ast::Statement::If(value) => unsupported_node_kind(&mut value.then_block.lock())
                .or_else(|| unsupported_node_kind(&mut value.else_block.lock())),
            ast::Statement::While(value) => unsupported_node_kind(&mut value.block.lock()),
            ast::Statement::Repeat(value) => unsupported_node_kind(&mut value.block.lock()),
            ast::Statement::NumericFor(value) => unsupported_node_kind(&mut value.block.lock()),
            ast::Statement::GenericFor(value) => unsupported_node_kind(&mut value.block.lock()),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
        let mut unsupported_value = None;
        statement.traverse_rvalues(&mut |rvalue| {
            if unsupported_value.is_some() {
                return;
            }
            match rvalue {
                ast::RValue::Conditional(_) => unsupported_value = Some("conditional expression"),
                ast::RValue::Literal(ast::Literal::Integer(_)) => {
                    unsupported_value = Some("integer literal")
                }
                ast::RValue::Binary(binary)
                    if binary.operation == ast::BinaryOperation::IDiv =>
                {
                    unsupported_value = Some("floor-division expression")
                }
                ast::RValue::Closure(closure) => {
                    unsupported_value = unsupported_node_kind(&mut closure.function.lock().body)
                }
                _ => {}
            }
        });
        if unsupported_value.is_some() {
            return unsupported_value;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{DecompilePhase, try_decompile_bytecode};

    enum Constant<'a> {
        Number(f64),
        String(&'a [u8]),
    }

    fn push_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn abc(opcode: u32, a: u32, b: u32, c: u32) -> u32 {
        opcode | (a << 6) | (c << 14) | (b << 23)
    }

    fn abx(opcode: u32, a: u32, bx: u32) -> u32 {
        opcode | (a << 6) | (bx << 14)
    }

    fn asbx(opcode: u32, a: u32, sbx: i32) -> u32 {
        abx(opcode, a, (sbx + 131_071) as u32)
    }

    fn chunk(code: &[u32], constants: &[Constant<'_>], maximum_stack_size: u8) -> Vec<u8> {
        chunk_with_signature(code, constants, maximum_stack_size, 0, 0, 0, 0, 0, &[])
    }

    fn chunk_with_signature(
        code: &[u32],
        constants: &[Constant<'_>],
        maximum_stack_size: u8,
        line_defined: u32,
        last_line_defined: u32,
        number_of_parameters: u8,
        vararg_flag: u8,
        number_of_upvalues: u8,
        upvalue_names: &[&[u8]],
    ) -> Vec<u8> {
        let mut output = vec![0x1b, b'L', b'u', b'a', 0x51, 0, 1, 4, 4, 4, 8, 0];
        push_u32(&mut output, 0);
        push_u32(&mut output, line_defined);
        push_u32(&mut output, last_line_defined);
        output.extend_from_slice(&[
            number_of_upvalues,
            number_of_parameters,
            vararg_flag,
            maximum_stack_size,
        ]);
        push_u32(&mut output, code.len() as u32);
        for word in code {
            push_u32(&mut output, *word);
        }
        push_u32(&mut output, constants.len() as u32);
        for constant in constants {
            match constant {
                Constant::Number(value) => {
                    output.push(3);
                    output.extend_from_slice(&value.to_le_bytes());
                }
                Constant::String(value) => {
                    output.push(4);
                    push_u32(&mut output, (value.len() + 1) as u32);
                    output.extend_from_slice(value);
                    output.push(0);
                }
            }
        }
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, upvalue_names.len() as u32);
        for name in upvalue_names {
            push_u32(&mut output, (name.len() + 1) as u32);
            output.extend_from_slice(name);
            output.push(0);
        }
        output
    }

    #[test]
    fn invalid_bytecode_returns_structured_deserialize_error() {
        let error = try_decompile_bytecode(b"not bytecode").unwrap_err();

        assert_eq!(error.phase, DecompilePhase::Deserialize);
        assert_eq!(error.invariant, "valid Lua 5.1 bytecode");
    }

    #[test]
    fn invalid_register_returns_structured_validation_error() {
        let bytecode = chunk(&[abc(0, 0, 1, 0), abc(30, 0, 1, 0)], &[], 1);

        let error = try_decompile_bytecode(&bytecode).unwrap_err();

        assert_eq!(error.phase, DecompilePhase::Validate);
        assert_eq!(error.function_id, Some(0));
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.invariant, "register inside declared stack");
    }

    #[test]
    fn missing_open_result_returns_structured_validation_error() {
        let bytecode = chunk(&[abc(30, 0, 0, 0)], &[], 1);

        let error = try_decompile_bytecode(&bytecode).unwrap_err();

        assert_eq!(error.phase, DecompilePhase::Validate);
        assert_eq!(error.function_id, Some(0));
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.invariant, "open result available in basic block");
    }

    #[test]
    fn branch_to_function_end_uses_empty_terminal_block() {
        let bytecode = chunk(
            &[asbx(22, 0, 1), abc(30, 0, 1, 0)],
            &[],
            1,
        );

        assert!(try_decompile_bytecode(&bytecode).is_ok());
    }

    #[test]
    fn table_slot_and_alias_recovery_produce_source_like_lua51() {
        let bytecode = chunk(
            &[abc(10, 0, 0, 0), abc(9, 0, 256, 257), abc(30, 0, 2, 0)],
            &[Constant::String(b"answer"), Constant::Number(7.0)],
            1,
        );

        let source = try_decompile_bytecode(&bytecode).unwrap();

        assert!(source.contains("return { answer = 7 }"), "{source}");
        assert!(!source.contains("continue"), "{source}");
        assert!(!source.contains("+="), "{source}");
        assert!(!source.contains(" if "), "{source}");
    }

    #[test]
    fn extended_setlist_decompiles_without_panicking() {
        let bytecode = chunk(
            &[
                abc(10, 0, 0, 0),
                abx(1, 1, 0),
                abc(34, 0, 1, 0),
                2,
                abc(30, 0, 2, 0),
            ],
            &[Constant::Number(7.0)],
            2,
        );

        let source = try_decompile_bytecode(&bytecode).unwrap();

        assert!(source.contains("[51] = 7"), "{source}");
    }

    #[test]
    fn dumped_legacy_vararg_root_keeps_its_implicit_arg_table() {
        let bytecode = chunk_with_signature(
            &[abc(30, 0, 2, 0)],
            &[],
            1,
            1,
            1,
            0,
            7,
            0,
            &[],
        );

        let source = try_decompile_bytecode(&bytecode).unwrap();

        assert!(source.starts_with("return function(...)"), "{source}");
        assert!(source.contains("return arg"), "{source}");
        assert!(!source.contains("local arg = nil"), "{source}");
    }

    #[test]
    fn dumped_root_upvalue_is_declared_before_the_returned_closure() {
        let bytecode = chunk_with_signature(
            &[abc(4, 0, 0, 0), abc(30, 0, 2, 0)],
            &[],
            1,
            0,
            0,
            0,
            0,
            1,
            &[b"x"],
        );

        let source = try_decompile_bytecode(&bytecode).unwrap();

        assert!(source.starts_with("local x = nil"), "{source}");
        assert!(source.contains("return function()"), "{source}");
        assert!(source.contains("return x"), "{source}");
    }
}
