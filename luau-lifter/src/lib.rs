mod deserializer;
mod error;
mod instruction;
mod lifter;
mod op_code;
mod profiling;

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

#[cfg(all(feature = "profiling", not(feature = "dhat-heap")))]
#[global_allocator]
static ALLOC: profiling::TrackingAllocator = profiling::TrackingAllocator;

#[cfg(all(
    feature = "mimalloc",
    not(feature = "profiling"),
    not(feature = "dhat-heap")
))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub use profiling::report_to_stderr as report_profile;

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
    let parsed = catch_phase(DecompilePhase::Deserialize, None, None, || {
        deserializer::deserialize(bytecode, encode_key)
    })?
    .map_err(|detail| {
        DecompileError::new(
            DecompilePhase::Deserialize,
            None,
            None,
            "valid Luau bytecode",
            detail,
        )
    })?;
    profiling::checkpoint("deserialized");
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

    let prototype_order = validate_prototype_graph(&chunk.functions)?;
    let expansion = validate_expansion_budget(
        &chunk.functions,
        chunk.main,
        &prototype_order,
        bytecode.len(),
    )?;
    decompile_chunk(chunk, expansion)
}

const MAX_SAFE_PROTOTYPE_DEPTH: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpansionPlan {
    instances: usize,
    instructions: usize,
    occurrences: Vec<usize>,
}

#[derive(Clone, Copy)]
struct PrototypeFrame {
    function: usize,
    nested_cursor: usize,
    constant_cursor: usize,
}

impl PrototypeFrame {
    fn new(function: usize) -> Self {
        Self {
            function,
            nested_cursor: 0,
            constant_cursor: 0,
        }
    }

    fn next_child(&mut self, function: &deserializer::function::Function) -> Option<usize> {
        if let Some(&child) = function.functions.get(self.nested_cursor) {
            self.nested_cursor += 1;
            return Some(child);
        }
        while let Some(constant) = function.constants.get(self.constant_cursor) {
            self.constant_cursor += 1;
            if let deserializer::constant::Constant::Closure(child) = constant {
                return Some(*child);
            }
        }
        None
    }
}

fn validate_prototype_graph(
    functions: &[deserializer::function::Function],
) -> Result<Vec<usize>, DecompileError> {
    let mut states = Vec::new();
    states.try_reserve_exact(functions.len()).map_err(|error| {
        DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "bounded prototype graph",
            error.to_string(),
        )
    })?;
    states.resize(functions.len(), 0u8);

    let mut postorder = Vec::new();
    postorder
        .try_reserve_exact(functions.len())
        .map_err(|error| {
            DecompileError::new(
                DecompilePhase::Validate,
                None,
                None,
                "bounded prototype graph",
                error.to_string(),
            )
        })?;

    let mut stack = Vec::new();
    stack.try_reserve(functions.len()).map_err(|error| {
        DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "bounded prototype graph",
            error.to_string(),
        )
    })?;

    for root in 0..functions.len() {
        if states[root] != 0 {
            continue;
        }
        states[root] = 1;
        stack.push(PrototypeFrame::new(root));
        while let Some(frame) = stack.last_mut() {
            let function_id = frame.function;
            let Some(child) = frame.next_child(&functions[function_id]) else {
                states[function_id] = 2;
                postorder.push(function_id);
                stack.pop();
                continue;
            };
            if child >= functions.len() {
                return Err(DecompileError::new(
                    DecompilePhase::Validate,
                    Some(function_id),
                    None,
                    "valid prototype graph",
                    format!("prototype references missing child {child}"),
                ));
            }
            match states[child] {
                0 => {
                    states[child] = 1;
                    stack.push(PrototypeFrame::new(child));
                }
                1 => {
                    return Err(DecompileError::new(
                        DecompilePhase::Validate,
                        Some(function_id),
                        None,
                        "acyclic prototype graph",
                        format!("prototype cycle reaches child {child}"),
                    ));
                }
                _ => {}
            }
        }
    }
    postorder.reverse();
    Ok(postorder)
}

fn closure_child(
    functions: &[deserializer::function::Function],
    function_id: usize,
    instruction: &instruction::Instruction,
) -> Result<Option<usize>, DecompileError> {
    let function = &functions[function_id];
    let instruction::Instruction::AD { op_code, d, .. } = instruction else {
        return Ok(None);
    };
    if !matches!(
        op_code,
        op_code::OpCode::LOP_NEWCLOSURE | op_code::OpCode::LOP_DUPCLOSURE
    ) {
        return Ok(None);
    }
    let index = usize::try_from(*d).map_err(|_| {
        DecompileError::new(
            DecompilePhase::Validate,
            Some(function_id),
            None,
            "valid closure reference",
            format!("negative closure operand {d}"),
        )
    })?;
    let child = match op_code {
        op_code::OpCode::LOP_NEWCLOSURE => function.functions.get(index).copied(),
        op_code::OpCode::LOP_DUPCLOSURE => function.constants.get(index).and_then(|constant| {
            if let deserializer::constant::Constant::Closure(child) = constant {
                Some(*child)
            } else {
                None
            }
        }),
        _ => unreachable!(),
    }
    .ok_or_else(|| {
        DecompileError::new(
            DecompilePhase::Validate,
            Some(function_id),
            None,
            "valid closure reference",
            format!("{op_code:?} operand {index} does not reference a prototype"),
        )
    })?;
    if child >= functions.len() {
        return Err(DecompileError::new(
            DecompilePhase::Validate,
            Some(function_id),
            None,
            "valid closure reference",
            format!("{op_code:?} references missing prototype {child}"),
        ));
    }
    Ok(Some(child))
}

fn validate_expansion_budget(
    functions: &[deserializer::function::Function],
    main: usize,
    prototype_order: &[usize],
    input_bytes: usize,
) -> Result<ExpansionPlan, DecompileError> {
    let mut occurrences = Vec::new();
    occurrences
        .try_reserve_exact(functions.len())
        .map_err(|error| {
            DecompileError::new(
                DecompilePhase::Validate,
                None,
                None,
                "bounded prototype expansion",
                error.to_string(),
            )
        })?;
    occurrences.resize(functions.len(), 0usize);
    occurrences[main] = 1;

    let mut depths = Vec::new();
    depths.try_reserve_exact(functions.len()).map_err(|error| {
        DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "bounded prototype expansion",
            error.to_string(),
        )
    })?;
    depths.resize(functions.len(), 0usize);
    depths[main] = 1;

    let mut instances = 0usize;
    let mut instructions = 0usize;
    for &function_id in prototype_order {
        let occurrence_count = occurrences[function_id];
        if occurrence_count == 0 {
            continue;
        }
        instances = instances.checked_add(occurrence_count).ok_or_else(|| {
            DecompileError::new(
                DecompilePhase::Validate,
                Some(function_id),
                None,
                "bounded prototype expansion",
                "expanded prototype count overflow",
            )
        })?;
        let expanded_instructions = functions[function_id]
            .instructions
            .len()
            .checked_mul(occurrence_count)
            .ok_or_else(|| {
                DecompileError::new(
                    DecompilePhase::Validate,
                    Some(function_id),
                    None,
                    "bounded prototype expansion",
                    "expanded instruction count overflow",
                )
            })?;
        instructions = instructions
            .checked_add(expanded_instructions)
            .ok_or_else(|| {
                DecompileError::new(
                    DecompilePhase::Validate,
                    Some(function_id),
                    None,
                    "bounded prototype expansion",
                    "expanded instruction count overflow",
                )
            })?;
        if instances > input_bytes || instructions > input_bytes {
            return Err(DecompileError::new(
                DecompilePhase::Validate,
                Some(function_id),
                None,
                "bounded prototype expansion",
                format!("expanded work exceeds the input-sized budget of {input_bytes} units"),
            ));
        }

        let child_depth = depths[function_id].checked_add(1).ok_or_else(|| {
            DecompileError::new(
                DecompilePhase::Validate,
                Some(function_id),
                None,
                "bounded prototype depth",
                "prototype depth overflow",
            )
        })?;
        for instruction in &functions[function_id].instructions {
            let Some(child) = closure_child(functions, function_id, instruction)? else {
                continue;
            };
            occurrences[child] = occurrences[child]
                .checked_add(occurrence_count)
                .ok_or_else(|| {
                    DecompileError::new(
                        DecompilePhase::Validate,
                        Some(function_id),
                        None,
                        "bounded prototype expansion",
                        "expanded prototype count overflow",
                    )
                })?;
            depths[child] = depths[child].max(child_depth);
            if depths[child] > MAX_SAFE_PROTOTYPE_DEPTH {
                return Err(DecompileError::new(
                    DecompilePhase::Validate,
                    Some(child),
                    None,
                    "bounded prototype depth",
                    format!(
                        "expanded closure depth {} exceeds the safe limit {MAX_SAFE_PROTOTYPE_DEPTH}",
                        depths[child]
                    ),
                ));
            }
        }
    }
    Ok(ExpansionPlan {
        instances,
        instructions,
        occurrences,
    })
}

fn release_lifted_prototype(function: &mut deserializer::function::Function) {
    function.instructions = Vec::new();
    function.constants = Vec::new();
    function.functions = Vec::new();
    function.line_info_delta = None;
    function.abs_line_info_delta = None;
    function.debug_locals = Vec::new();
    function.debug_upvalues = Vec::new();
    function.feedback = Vec::new();
}

fn decompile_chunk(
    mut chunk: deserializer::chunk::Chunk,
    expansion: ExpansionPlan,
) -> Result<String, DecompileError> {
    let ExpansionPlan {
        instances: planned_instances,
        instructions: planned_instructions,
        occurrences: mut remaining_instances,
    } = expansion;
    let function_count = chunk.functions.len();
    let main = Arc::<Mutex<ast::Function>>::default();
    let mut stack = Vec::new();
    stack.try_reserve_exact(function_count).map_err(|error| {
        DecompileError::new(
            DecompilePhase::Lift,
            None,
            None,
            "bounded prototype expansion",
            error.to_string(),
        )
    })?;
    stack.push((main.clone(), chunk.main));
    let mut scheduled_instances = 1usize;
    let mut processed_instructions = 0usize;

    let mut decompiled_upvalues = FxHashMap::default();
    decompiled_upvalues
        .try_reserve(function_count.min(planned_instances))
        .map_err(|error| {
            DecompileError::new(
                DecompilePhase::Lift,
                None,
                None,
                "bounded prototype expansion",
                error.to_string(),
            )
        })?;
    while let Some((ast_func, func_id)) = stack.pop() {
        let remaining = remaining_instances.get_mut(func_id).ok_or_else(|| {
            DecompileError::new(
                DecompilePhase::Validate,
                Some(func_id),
                None,
                "bounded prototype expansion",
                "scheduled prototype is outside the validated expansion plan",
            )
        })?;
        if *remaining == 0 {
            return Err(DecompileError::new(
                DecompilePhase::Validate,
                Some(func_id),
                None,
                "bounded prototype expansion",
                "scheduled prototype exceeds its validated occurrence count",
            ));
        }
        processed_instructions = processed_instructions
            .checked_add(chunk.functions[func_id].instructions.len())
            .ok_or_else(|| {
                DecompileError::new(
                    DecompilePhase::Lift,
                    Some(func_id),
                    None,
                    "bounded prototype expansion",
                    "processed instruction count overflow",
                )
            })?;
        let (function, upvalues_in, child_functions) =
            catch_phase(DecompilePhase::Lift, Some(func_id), None, || {
                Lifter::lift(&chunk.functions, &chunk.string_table, func_id)
            })?;
        *remaining -= 1;
        if *remaining == 0 {
            release_lifted_prototype(&mut chunk.functions[func_id]);
        }
        let mut children = Vec::new();
        children
            .try_reserve_exact(child_functions.len())
            .map_err(|error| {
                DecompileError::new(
                    DecompilePhase::Lift,
                    Some(func_id),
                    None,
                    "bounded prototype expansion",
                    error.to_string(),
                )
            })?;
        children.extend(
            child_functions
                .into_iter()
                .map(|(function, id)| (function.0, id)),
        );
        children.sort_unstable_by_key(|(_, id)| *id);
        stack.try_reserve(children.len()).map_err(|error| {
            DecompileError::new(
                DecompilePhase::Lift,
                Some(func_id),
                None,
                "bounded prototype expansion",
                error.to_string(),
            )
        })?;
        for (function, child_id) in children {
            scheduled_instances = scheduled_instances.checked_add(1).ok_or_else(|| {
                DecompileError::new(
                    DecompilePhase::Lift,
                    Some(func_id),
                    None,
                    "bounded prototype expansion",
                    "scheduled prototype count overflow",
                )
            })?;
            if scheduled_instances > planned_instances {
                return Err(DecompileError::new(
                    DecompilePhase::Validate,
                    Some(func_id),
                    None,
                    "bounded prototype expansion",
                    "scheduled prototype count exceeds the validated expansion plan",
                ));
            }
            stack.push((function, child_id));
        }
        let (function, function_upvalues) = decompile_function(ast_func, function, upvalues_in)?;
        decompiled_upvalues.try_reserve(1).map_err(|error| {
            DecompileError::new(
                DecompilePhase::Lift,
                Some(func_id),
                None,
                "bounded prototype expansion",
                error.to_string(),
            )
        })?;
        decompiled_upvalues.insert(function, function_upvalues);
    }
    if scheduled_instances != planned_instances
        || processed_instructions != planned_instructions
        || remaining_instances.iter().any(|remaining| *remaining != 0)
    {
        return Err(DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "bounded prototype expansion",
            format!(
                "validated {} instances/{} instructions but processed \
                 {scheduled_instances}/{processed_instructions}",
                planned_instances, planned_instructions
            ),
        ));
    }
    profiling::checkpoint("functions-decompiled");
    drop(chunk);
    profiling::checkpoint("chunk-dropped");

    let main = ByAddress(main);
    decompiled_upvalues.remove(&main);
    let mut body = catch_phase(DecompilePhase::Link, None, None, || {
        let mut body = Arc::try_unwrap(main.0).unwrap().into_inner().body;
        link_upvalues(&mut body, &mut decompiled_upvalues);
        body
    })?;
    profiling::checkpoint("linked");
    drop(decompiled_upvalues);
    profiling::checkpoint("upvalues-dropped");
    if block_contains_unsupported_nodes(&mut body) {
        return Err(DecompileError::new(
            DecompilePhase::Validate,
            None,
            None,
            "source-level AST",
            "reconstruction left internal goto, label, or set-list nodes",
        ));
    }
    profiling::checkpoint("validated");
    catch_phase(DecompilePhase::Format, None, None, || {
        ast::recover_function_syntax(&mut body);
        profiling::checkpoint("function-syntax-recovered");
        name_locals(&mut body, false);
        profiling::checkpoint("locals-named");
        let source = body.to_string();
        profiling::checkpoint("formatted");
        source
    })
}

fn block_contains_unsupported_nodes(block: &mut ast::Block) -> bool {
    for statement in &mut block.0 {
        if matches!(
            statement,
            ast::Statement::Goto(_) | ast::Statement::Label(_) | ast::Statement::SetList(_)
        ) {
            return true;
        }

        let nested_internal_node = match statement {
            ast::Statement::If(r#if) => {
                block_contains_unsupported_nodes(&mut r#if.then_block.lock())
                    || block_contains_unsupported_nodes(&mut r#if.else_block.lock())
            }
            ast::Statement::While(r#while) => {
                block_contains_unsupported_nodes(&mut r#while.block.lock())
            }
            ast::Statement::Repeat(repeat) => {
                block_contains_unsupported_nodes(&mut repeat.block.lock())
            }
            ast::Statement::NumericFor(numeric_for) => {
                block_contains_unsupported_nodes(&mut numeric_for.block.lock())
            }
            ast::Statement::GenericFor(generic_for) => {
                block_contains_unsupported_nodes(&mut generic_for.block.lock())
            }
            _ => false,
        };
        if nested_internal_node {
            return true;
        }

        let mut closure_internal_node = false;
        statement.traverse_rvalues(&mut |rvalue| {
            if !closure_internal_node && let ast::RValue::Closure(closure) = rvalue {
                closure_internal_node =
                    block_contains_unsupported_nodes(&mut closure.function.lock().body);
            }
        });
        if closure_internal_node {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod output_tests {
    use super::{
        ExpansionPlan, MAX_SAFE_PROTOTYPE_DEPTH, block_contains_unsupported_nodes,
        validate_expansion_budget, validate_prototype_graph,
    };

    fn prototype(functions: Vec<usize>) -> super::deserializer::function::Function {
        super::deserializer::function::Function {
            max_stack_size: 1,
            num_parameters: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            instructions: Vec::new(),
            constants: Vec::new(),
            functions,
            line_defined: 0,
            function_name: 0,
            line_gap_log2: None,
            line_info_delta: None,
            abs_line_info_delta: None,
            debug_locals: Vec::new(),
            debug_upvalues: Vec::new(),
            feedback: Vec::new(),
            cost: None,
        }
    }

    fn closure_prototype(
        functions: Vec<usize>,
        child_slots: &[usize],
    ) -> super::deserializer::function::Function {
        let mut prototype = prototype(functions);
        prototype.instructions = child_slots
            .iter()
            .map(|&slot| super::instruction::Instruction::AD {
                op_code: super::op_code::OpCode::LOP_NEWCLOSURE,
                a: 0,
                d: i16::try_from(slot).unwrap(),
                aux: 0,
            })
            .collect();
        prototype
    }

    fn duplicate_closure_prototype(child: usize) -> super::deserializer::function::Function {
        let mut prototype = prototype(Vec::new());
        prototype.constants = vec![super::deserializer::constant::Constant::Closure(child)];
        prototype.instructions = vec![super::instruction::Instruction::AD {
            op_code: super::op_code::OpCode::LOP_DUPCLOSURE,
            a: 0,
            d: 0,
            aux: 0,
        }];
        prototype
    }

    #[test]
    fn rejects_unsupported_jump_nodes() {
        let label = ast::Label("exit".to_owned());
        let mut body = ast::Block(vec![ast::Goto::new(label).into()]);

        assert!(block_contains_unsupported_nodes(&mut body));
    }

    #[test]
    fn rejects_internal_set_list_nodes() {
        let table = ast::RcLocal::default();
        let mut body = ast::Block(vec![
            ast::SetList::new(table, 1, vec![ast::Literal::Number(1.0).into()], None).into(),
        ]);

        assert!(block_contains_unsupported_nodes(&mut body));
    }

    #[test]
    fn rejects_cyclic_prototype_graph() {
        let functions = vec![prototype(vec![1]), prototype(vec![0])];

        let error = validate_prototype_graph(&functions).unwrap_err();

        assert_eq!(error.phase, super::DecompilePhase::Validate);
        assert_eq!(error.invariant, "acyclic prototype graph");
    }

    #[test]
    fn accepts_shared_acyclic_prototype_graph() {
        let functions = vec![
            prototype(vec![1, 2]),
            prototype(vec![2]),
            prototype(Vec::new()),
        ];

        validate_prototype_graph(&functions).unwrap();
    }

    #[test]
    fn budgets_repeated_shared_prototype_instantiation() {
        let functions = vec![
            closure_prototype(vec![1, 2], &[0, 1]),
            closure_prototype(vec![2], &[0]),
            prototype(Vec::new()),
        ];
        let order = validate_prototype_graph(&functions).unwrap();

        let plan = validate_expansion_budget(&functions, 0, &order, 100).unwrap();

        assert_eq!(
            plan,
            ExpansionPlan {
                instances: 4,
                instructions: 3,
                occurrences: vec![1, 1, 2],
            }
        );
    }

    #[test]
    fn budgets_duplicate_closure_constant_instantiation() {
        let functions = vec![duplicate_closure_prototype(1), prototype(Vec::new())];
        let order = validate_prototype_graph(&functions).unwrap();

        let plan = validate_expansion_budget(&functions, 0, &order, 100).unwrap();

        assert_eq!(
            plan,
            ExpansionPlan {
                instances: 2,
                instructions: 1,
                occurrences: vec![1, 1],
            }
        );
    }

    #[test]
    fn rejects_duplicate_closure_constant_cycle() {
        let functions = vec![
            duplicate_closure_prototype(1),
            duplicate_closure_prototype(0),
        ];

        assert!(validate_prototype_graph(&functions).is_err());
    }

    #[test]
    fn rejects_exponential_prototype_work_past_input_sized_budget() {
        let mut functions = Vec::new();
        for function_id in 0..8 {
            functions.push(if function_id == 7 {
                prototype(Vec::new())
            } else {
                closure_prototype(vec![function_id + 1], &[0, 0])
            });
        }
        let order = validate_prototype_graph(&functions).unwrap();

        let error = validate_expansion_budget(&functions, 0, &order, 32).unwrap_err();

        assert_eq!(error.invariant, "bounded prototype expansion");
    }

    #[test]
    fn rejects_prototype_depth_past_safe_recursive_phase_limit() {
        let mut functions = Vec::new();
        for function_id in 0..=MAX_SAFE_PROTOTYPE_DEPTH {
            functions.push(if function_id == MAX_SAFE_PROTOTYPE_DEPTH {
                prototype(Vec::new())
            } else {
                closure_prototype(vec![function_id + 1], &[0])
            });
        }
        let order = validate_prototype_graph(&functions).unwrap();

        let error = validate_expansion_budget(&functions, 0, &order, usize::MAX).unwrap_err();

        assert_eq!(error.invariant, "bounded prototype depth");
    }
}

#[cfg(test)]
mod structured_error_tests {
    use super::{DecompileError, DecompilePhase, catch_phase, decompile_bytecode, ssa_error};

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

    #[test]
    fn bounded_ssa_resource_error_retains_function_context() {
        let error = ssa_error(
            31,
            cfg::ssa::SsaError::Upvalues(cfg::ssa::upvalues::UpvalueAnalysisError::Resource(
                "capacity".to_owned(),
            )),
        );

        assert_eq!(error.phase, DecompilePhase::Ssa);
        assert_eq!(error.function_id, Some(31));
        assert_eq!(error.instruction, None);
        assert_eq!(error.invariant, "bounded SSA analysis");
        assert_eq!(
            error.detail,
            "unable to reserve bounded upvalue state: capacity"
        );
    }
}

fn decompile_function(
    ast_function: Arc<Mutex<ast::Function>>,
    mut function: Function,
    upvalues_in: Vec<ast::RcLocal>,
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
                .flat_map(|(i, g)| g.into_iter().map(move |u| (u, i.clone())))
                .collect::<IndexMap<_, _>>();
            // TODO: do we even need this?
            let local_to_group = local_groups
                .into_iter()
                .enumerate()
                .flat_map(|(i, g)| g.into_iter().map(move |l| (l, i)))
                .collect::<FxHashMap<_, _>>();
            Ok((local_count, upvalue_to_group, local_to_group))
        },
    )?;
    let (local_count, upvalue_to_group, local_to_group) =
        ssa_result.map_err(|error| ssa_error(function_id, error))?;

    let recovery_report = catch_phase(DecompilePhase::Structure, Some(function_id), None, || {
        let mut scheduler = cfg::recovery::PassScheduler::new(32);
        scheduler.add_pass("structure-jumps", |function| {
            let dominators = cfg::metrics::time(cfg::metrics::Metric::Dominators, || {
                simple_fast(function.graph(), function.entry().unwrap())
            });
            cfg::metrics::time(cfg::metrics::Metric::StructureJumps, || {
                if structure_jumps(function, &dominators) {
                    cfg::recovery::PassChange::cfg().union(cfg::recovery::PassChange::ast())
                } else {
                    cfg::recovery::PassChange::none()
                }
            })
        });
        scheduler.add_pass("inline", |function| {
            #[cfg(feature = "verify-inline-change")]
            let before = cfg::recovery::structural_fingerprint(function);

            let changed = cfg::metrics::time(cfg::metrics::Metric::Inline, || {
                ssa::inline::inline(function, &local_to_group, &upvalue_to_group)
            });

            // The reported flag must agree with the fingerprint in both
            // directions. A false negative ends a round early; a false
            // positive drives a round that changes nothing, which the
            // scheduler reports as a repeated state and fails the decompile.
            #[cfg(feature = "verify-inline-change")]
            {
                let after = cfg::recovery::structural_fingerprint(function);
                assert_eq!(
                    changed,
                    before != after,
                    "inline reported changed={changed} but the structural \
                     fingerprint disagrees"
                );
            }

            if changed {
                cfg::recovery::PassChange::dataflow().union(cfg::recovery::PassChange::ast())
            } else {
                cfg::recovery::PassChange::none()
            }
        });
        scheduler.add_pass("structure-conditionals", |function| {
            cfg::metrics::time(cfg::metrics::Metric::StructureConditionals, || {
                if structure_conditionals(function) {
                    cfg::recovery::PassChange::cfg()
                        .union(cfg::recovery::PassChange::dataflow())
                        .union(cfg::recovery::PassChange::ast())
                } else {
                    cfg::recovery::PassChange::none()
                }
            })
        });
        scheduler.add_pass("remove-unnecessary-params", |function| {
            cfg::metrics::time(cfg::metrics::Metric::RemoveParams, || {
                let mut local_map = FxHashMap::default();
                if ssa::construct::remove_unnecessary_params(function, &mut local_map) {
                    ssa::construct::apply_local_map(function, local_map);
                    cfg::recovery::PassChange::cfg()
                        .union(cfg::recovery::PassChange::dataflow())
                        .union(cfg::recovery::PassChange::ast())
                } else {
                    cfg::recovery::PassChange::none()
                }
            })
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
                upvalue_to_group,
                upvalues_in.iter().cloned().collect(),
                local_count,
            )
            .destruct();
        },
    )?;
    debug_assert_eq!(recovery_facts.function_id(), function_id);

    let (params, is_variadic, mut block) =
        catch_phase(DecompilePhase::Restructure, Some(function_id), None, || {
            let params = std::mem::take(&mut function.parameters);
            let is_variadic = function.is_variadic;
            let block: ast::Block = restructure::lift(function, &recovery_facts).into();
            (params, is_variadic, block)
        })?;

    catch_phase(DecompilePhase::AstRecovery, Some(function_id), None, || {
        ast::eliminate_aliases_with_protected(&mut block, &upvalues_in);
        ast::recover_expressions_with_protected(&mut block, &upvalues_in);
        ast::cleanup_control_flow(&mut block);
    })?;

    let block = Arc::new(Mutex::new(block));
    catch_phase(DecompilePhase::Declaration, Some(function_id), None, || {
        let initially_visible = upvalues_in.iter().chain(params.iter()).cloned().collect();
        LocalDeclarer::default().declare_locals(
            // TODO: why does block.clone() not work?
            Arc::clone(&block),
            &initially_visible,
        );
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
        let mut ast_function = ast_function.lock();
        ast_function.body = Arc::try_unwrap(block).unwrap().into_inner();
        ast_function.parameters = params;
        ast_function.is_variadic = is_variadic;
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
