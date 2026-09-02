use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap},
};

use either::Either;
use lua51_deserializer::{
    Function, Instruction, Value,
    argument::{Constant, Register, RegisterOrConstant},
};

use crate::{DecompileError, DecompilePhase};

const MAX_FUNCTION_DEPTH: usize = 256;
const VARARG_HASARG: u8 = 1;
const VARARG_ISVARARG: u8 = 2;
const VARARG_NEEDSARG: u8 = 4;
const VARARG_MASK: u8 = VARARG_HASARG | VARARG_ISVARARG | VARARG_NEEDSARG;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidationPlan {
    pub instances: usize,
    pub instructions: usize,
}

pub(crate) fn function_tree(
    root: &Function<'_>,
    input_bytes: usize,
) -> Result<ValidationPlan, DecompileError> {
    let mut stack = vec![(root, 1usize)];
    let mut function_id = 0usize;
    let mut instructions = 0usize;
    while let Some((function, depth)) = stack.pop() {
        if depth > MAX_FUNCTION_DEPTH {
            return Err(error(
                function_id,
                None,
                "bounded prototype depth",
                format!("prototype depth exceeds {MAX_FUNCTION_DEPTH}"),
            ));
        }
        instructions = instructions
            .checked_add(function.code.len())
            .ok_or_else(|| {
                error(
                    function_id,
                    None,
                    "bounded prototype work",
                    "instruction count overflow",
                )
            })?;
        if instructions > input_bytes {
            return Err(error(
                function_id,
                None,
                "bounded prototype work",
                format!("decoded instructions exceed input-sized budget of {input_bytes}"),
            ));
        }
        validate_function(function, function_id)?;
        for child in function.closures.iter().rev() {
            stack.push((child, depth + 1));
        }
        function_id = function_id
            .checked_add(1)
            .ok_or_else(|| error(0, None, "bounded prototype work", "function count overflow"))?;
    }
    expansion_plan(root, input_bytes)
}

fn expansion_plan(
    root: &Function<'_>,
    input_bytes: usize,
) -> Result<ValidationPlan, DecompileError> {
    let mut stack = vec![(root, 1usize)];
    let mut plan = ValidationPlan {
        instances: 0,
        instructions: 0,
    };
    while let Some((function, depth)) = stack.pop() {
        if depth > MAX_FUNCTION_DEPTH {
            return Err(error(
                plan.instances,
                None,
                "bounded prototype expansion depth",
                format!("expanded prototype depth exceeds {MAX_FUNCTION_DEPTH}"),
            ));
        }
        plan.instances = plan.instances.checked_add(1).ok_or_else(|| {
            error(
                0,
                None,
                "bounded prototype expansion",
                "expanded prototype count overflow",
            )
        })?;
        plan.instructions = plan
            .instructions
            .checked_add(function.code.len())
            .ok_or_else(|| {
                error(
                    plan.instances - 1,
                    None,
                    "bounded prototype expansion",
                    "expanded instruction count overflow",
                )
            })?;
        if plan.instructions > input_bytes {
            return Err(error(
                plan.instances - 1,
                None,
                "bounded prototype expansion",
                format!(
                    "expanded instruction work exceeds input-sized budget of {input_bytes}"
                ),
            ));
        }

        let child_count = function
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::Closure { .. }))
            .count();
        stack.try_reserve(child_count).map_err(|allocation| {
            error(
                plan.instances - 1,
                None,
                "bounded prototype expansion",
                allocation.to_string(),
            )
        })?;
        for instruction in function.code.iter().rev() {
            if let Instruction::Closure {
                function: child, ..
            } = instruction
            {
                stack.push((&function.closures[child.0 as usize], depth + 1));
            }
        }
    }
    Ok(plan)
}

fn validate_function(function: &Function<'_>, function_id: usize) -> Result<(), DecompileError> {
    if function.code.is_empty() {
        return Err(error(
            function_id,
            None,
            "non-empty function",
            "function has no instructions",
        ));
    }
    if function.number_of_parameters > function.maximum_stack_size {
        return Err(error(
            function_id,
            None,
            "parameters fit declared stack",
            format!(
                "{} parameters exceed stack size {}",
                function.number_of_parameters, function.maximum_stack_size
            ),
        ));
    }
    let has_arg = function.vararg_flag & VARARG_HASARG != 0;
    let is_vararg = function.vararg_flag & VARARG_ISVARARG != 0;
    let needs_arg = function.vararg_flag & VARARG_NEEDSARG != 0;
    if function.vararg_flag & !VARARG_MASK != 0
        || (has_arg && !is_vararg)
        || (needs_arg && !has_arg)
    {
        return Err(error(
            function_id,
            None,
            "valid Lua 5.1 vararg flags",
            format!("unsupported vararg flag combination {:#04x}", function.vararg_flag),
        ));
    }
    if has_arg && function.number_of_parameters >= function.maximum_stack_size {
        return Err(error(
            function_id,
            None,
            "legacy arg register fits declared stack",
            format!(
                "legacy arg register {} exceeds stack size {}",
                function.number_of_parameters, function.maximum_stack_size
            ),
        ));
    }
    if !function.positions.is_empty() && function.positions.len() != function.code.len() {
        return Err(error(
            function_id,
            None,
            "line information matches code size",
            format!(
                "{} line entries do not match {} instruction words",
                function.positions.len(),
                function.code.len()
            ),
        ));
    }
    for constant in &function.constants {
        if matches!(constant, Value::Number(value) if !value.is_finite()) {
            return Err(error(
                function_id,
                None,
                "finite Lua number constant",
                "non-finite numeric constant cannot be emitted as Lua 5.1 source",
            ));
        }
    }
    let mut previous_local_start = 0u32;
    let mut active_local_ends = BinaryHeap::new();
    for local in &function.locals {
        if local.range.start > local.range.end || local.range.end as usize > function.code.len() {
            return Err(error(
                function_id,
                None,
                "debug local range in function",
                format!(
                    "debug local range {}..{} exceeds code size {}",
                    local.range.start,
                    local.range.end,
                    function.code.len()
                ),
            ));
        }
        if local.range.start < previous_local_start {
            return Err(error(
                function_id,
                None,
                "debug locals ordered by activation",
                "debug local start positions are not monotonic",
            ));
        }
        previous_local_start = local.range.start;
        while active_local_ends
            .peek()
            .is_some_and(|Reverse(end)| *end <= local.range.start)
        {
            active_local_ends.pop();
        }
        if local.range.start < local.range.end {
            if active_local_ends.len() >= usize::from(function.maximum_stack_size) {
                return Err(error(
                    function_id,
                    None,
                    "debug locals fit declared stack",
                    format!(
                        "more than {} debug locals are simultaneously active",
                        function.maximum_stack_size
                    ),
                ));
            }
            active_local_ends.push(Reverse(local.range.end));
        }
    }
    for (pc, instruction) in function.code.iter().enumerate() {
        validate_instruction(function, function_id, pc, instruction)?;
    }
    validate_auxiliary_targets(function, function_id)?;
    validate_open_results(function, function_id)?;
    Ok(())
}

fn control_flow_block_starts(
    function: &Function<'_>,
    function_id: usize,
) -> Result<BTreeSet<usize>, DecompileError> {
    let mut block_starts = BTreeSet::from([0usize]);
    for (pc, instruction) in function.code.iter().enumerate() {
        match instruction {
            Instruction::LoadBoolean {
                skip_next: true, ..
            }
            | Instruction::Equal { .. }
            | Instruction::LessThan { .. }
            | Instruction::LessThanOrEqual { .. }
            | Instruction::Test { .. }
            | Instruction::TestSet { .. }
            | Instruction::IterateGenericForLoop { .. } => {
                block_starts.insert(pc + 1);
                block_starts.insert(pc + 2);
            }
            Instruction::Jump(skip)
            | Instruction::IterateNumericForLoop { skip, .. }
            | Instruction::InitNumericForLoop { skip, .. } => {
                let target = jump_target(pc, *skip).ok_or_else(|| {
                    error(
                        function_id,
                        Some(pc),
                        "jump target in function",
                        "jump target overflow",
                    )
                })?;
                block_starts.insert(target);
                block_starts.insert(pc + 1);
            }
            Instruction::Return(..) => {
                block_starts.insert(pc + 1);
            }
            _ => {}
        }
    }
    Ok(block_starts)
}

fn validate_auxiliary_targets(
    function: &Function<'_>,
    function_id: usize,
) -> Result<(), DecompileError> {
    let mut auxiliary = BTreeSet::new();
    for (pc, instruction) in function.code.iter().enumerate() {
        match instruction {
            Instruction::ExtraWord(_) => {
                auxiliary.insert(pc);
            }
            Instruction::Closure {
                function: child, ..
            } => {
                let child = &function.closures[child.0 as usize];
                auxiliary.extend((pc + 1)..(pc + 1 + usize::from(child.number_of_upvalues)));
            }
            _ => {}
        }
    }
    for target in control_flow_block_starts(function, function_id)? {
        if auxiliary.contains(&target) {
            return Err(error(
                function_id,
                Some(target),
                "control flow targets executable instruction",
                "control-flow block starts inside closure capture or SETLIST extension data",
            ));
        }
    }
    Ok(())
}

fn validate_open_results(
    function: &Function<'_>,
    function_id: usize,
) -> Result<(), DecompileError> {
    let starts = control_flow_block_starts(function, function_id)?
        .into_iter()
        .collect::<Vec<_>>();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts
            .get(index + 1)
            .copied()
            .unwrap_or(function.code.len());
        if start >= function.code.len() {
            continue;
        }

        let mut open_result = None;
        for (pc, instruction) in function.code[start..end].iter().enumerate() {
            let pc = start + pc;
            let consumes_open_result = matches!(
                instruction,
                Instruction::Call { arguments: 0, .. }
                    | Instruction::TailCall { arguments: 0, .. }
                    | Instruction::Return(_, 0)
                    | Instruction::SetList {
                        number_of_elements: 0,
                        ..
                    }
            );
            if open_result.is_some() && !consumes_open_result {
                return Err(error(
                    function_id,
                    Some(pc),
                    "open result consumed by next instruction",
                    format!(
                        "instruction does not consume variable-width result produced at {}",
                        open_result.unwrap()
                    ),
                ));
            }
            if consumes_open_result && open_result.is_none() {
                return Err(error(
                    function_id,
                    Some(pc),
                    "open result available in basic block",
                    "instruction consumes a variable-width result that was not produced in this block",
                ));
            }
            if consumes_open_result {
                open_result = None;
            }

            if matches!(instruction, Instruction::TailCall { .. }) {
                break;
            }

            if matches!(
                instruction,
                Instruction::Call {
                    return_values: 0,
                    ..
                } | Instruction::VarArg(_, 0)
            ) {
                open_result = Some(pc);
            }
        }
        if let Some(producer) = open_result {
            return Err(error(
                function_id,
                Some(producer),
                "open result consumed in basic block",
                "variable-width result reaches a control-flow boundary without a consumer",
            ));
        }
    }
    Ok(())
}

fn validate_instruction(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    instruction: &Instruction,
) -> Result<(), DecompileError> {
    match instruction {
        Instruction::Move {
            destination,
            source,
        } => {
            register(function, function_id, pc, *destination)?;
            register(function, function_id, pc, *source)?;
        }
        Instruction::LoadConstant {
            destination,
            source,
        } => {
            register(function, function_id, pc, *destination)?;
            constant(function, function_id, pc, *source)?;
        }
        Instruction::LoadBoolean {
            destination,
            skip_next,
            ..
        } => {
            register(function, function_id, pc, *destination)?;
            if *skip_next {
                conditional_fallthrough(function, function_id, pc)?;
            }
        }
        Instruction::LoadNil(values) => {
            for value in values {
                register(function, function_id, pc, *value)?;
            }
        }
        Instruction::GetUpvalue {
            destination,
            upvalue: index,
        } => {
            register(function, function_id, pc, *destination)?;
            upvalue(function, function_id, pc, index.0)?;
        }
        Instruction::GetGlobal {
            destination,
            global,
        } => {
            register(function, function_id, pc, *destination)?;
            string_constant(function, function_id, pc, *global)?;
        }
        Instruction::GetIndex {
            destination,
            object,
            key,
        } => {
            register(function, function_id, pc, *destination)?;
            register(function, function_id, pc, *object)?;
            register_or_constant(function, function_id, pc, key)?;
        }
        Instruction::SetGlobal { destination, value } => {
            string_constant(function, function_id, pc, *destination)?;
            register(function, function_id, pc, *value)?;
        }
        Instruction::SetUpvalue {
            destination,
            source,
        } => {
            upvalue(function, function_id, pc, destination.0)?;
            register(function, function_id, pc, *source)?;
        }
        Instruction::SetIndex { object, key, value } => {
            register(function, function_id, pc, *object)?;
            register_or_constant(function, function_id, pc, key)?;
            register_or_constant(function, function_id, pc, value)?;
        }
        Instruction::NewTable { destination, .. } => {
            register(function, function_id, pc, *destination)?;
        }
        Instruction::PrepMethodCall {
            destination,
            self_arg,
            object,
            method,
        } => {
            register(function, function_id, pc, *destination)?;
            register(function, function_id, pc, *self_arg)?;
            register(function, function_id, pc, *object)?;
            register_or_constant(function, function_id, pc, method)?;
        }
        Instruction::Add {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Sub {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Mul {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Div {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Mod {
            destination,
            lhs,
            rhs,
        }
        | Instruction::Pow {
            destination,
            lhs,
            rhs,
        } => {
            register(function, function_id, pc, *destination)?;
            register_or_constant(function, function_id, pc, lhs)?;
            register_or_constant(function, function_id, pc, rhs)?;
        }
        Instruction::Minus {
            destination,
            operand,
        }
        | Instruction::Not {
            destination,
            operand,
        }
        | Instruction::Length {
            destination,
            operand,
        } => {
            register(function, function_id, pc, *destination)?;
            register(function, function_id, pc, *operand)?;
        }
        Instruction::Concatenate {
            destination,
            operands,
        } => {
            register(function, function_id, pc, *destination)?;
            if operands.len() < 2 {
                return Err(error(
                    function_id,
                    Some(pc),
                    "concatenation operand span",
                    "concatenation requires at least two registers",
                ));
            }
            for operand in operands {
                register(function, function_id, pc, *operand)?;
            }
        }
        Instruction::Jump(skip) => jump(function, function_id, pc, *skip)?,
        Instruction::Equal { lhs, rhs, .. }
        | Instruction::LessThan { lhs, rhs, .. }
        | Instruction::LessThanOrEqual { lhs, rhs, .. } => {
            register_or_constant(function, function_id, pc, lhs)?;
            register_or_constant(function, function_id, pc, rhs)?;
            conditional_fallthrough(function, function_id, pc)?;
        }
        Instruction::Test { value, .. } => {
            register(function, function_id, pc, *value)?;
            conditional_fallthrough(function, function_id, pc)?;
        }
        Instruction::TestSet {
            destination, value, ..
        } => {
            register(function, function_id, pc, *destination)?;
            register(function, function_id, pc, *value)?;
            conditional_fallthrough(function, function_id, pc)?;
            if !matches!(function.code.get(pc + 1), Some(Instruction::Jump(_))) {
                return Err(error(
                    function_id,
                    Some(pc),
                    "TESTSET followed by jump",
                    "TESTSET requires the compiler-form JMP successor",
                ));
            }
        }
        Instruction::Call {
            function: callee,
            arguments,
            return_values,
        } => {
            register(function, function_id, pc, *callee)?;
            if *arguments > 0 {
                window(function, function_id, pc, callee.0, usize::from(*arguments))?;
            }
            if *return_values > 0 {
                window(
                    function,
                    function_id,
                    pc,
                    callee.0,
                    usize::from(*return_values - 1),
                )?;
            }
        }
        Instruction::TailCall {
            function: callee,
            arguments,
        } => {
            register(function, function_id, pc, *callee)?;
            if *arguments > 0 {
                window(function, function_id, pc, callee.0, usize::from(*arguments))?;
            }
        }
        Instruction::Return(start, count) => {
            register(function, function_id, pc, *start)?;
            if *count > 0 {
                window(function, function_id, pc, start.0, usize::from(*count - 1))?;
            }
        }
        Instruction::IterateNumericForLoop { control, skip }
        | Instruction::InitNumericForLoop { control, skip } => {
            if control.len() != 4 {
                return Err(error(
                    function_id,
                    Some(pc),
                    "numeric-for register span",
                    format!("expected four registers, found {}", control.len()),
                ));
            }
            for value in control {
                register(function, function_id, pc, *value)?;
            }
            jump(function, function_id, pc, *skip)?;
        }
        Instruction::IterateGenericForLoop {
            generator,
            state,
            internal_control,
            vars,
        } => {
            register(function, function_id, pc, *generator)?;
            register(function, function_id, pc, *state)?;
            register(function, function_id, pc, *internal_control)?;
            if vars.is_empty() {
                return Err(error(
                    function_id,
                    Some(pc),
                    "generic-for result span",
                    "generic-for has no control result",
                ));
            }
            for value in vars {
                register(function, function_id, pc, *value)?;
            }
            conditional_fallthrough(function, function_id, pc)?;
        }
        Instruction::SetList {
            table,
            number_of_elements,
            block_number,
        } => {
            register(function, function_id, pc, *table)?;
            if *block_number == 0 {
                return Err(error(
                    function_id,
                    Some(pc),
                    "normalized SETLIST block",
                    "SETLIST extension is missing",
                ));
            }
            let block = usize::try_from(*block_number).map_err(|_| {
                error(
                    function_id,
                    Some(pc),
                    "SETLIST output index",
                    "SETLIST block does not fit the target platform",
                )
            })?;
            block
                .checked_sub(1)
                .and_then(|value| value.checked_mul(50))
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| {
                    error(
                        function_id,
                        Some(pc),
                        "SETLIST output index",
                        "SETLIST output index overflow",
                    )
                })?;
            if *number_of_elements > 0 {
                let start = table.0.checked_add(1).ok_or_else(|| {
                    error(
                        function_id,
                        Some(pc),
                        "SETLIST register span",
                        "SETLIST register start overflow",
                    )
                })?;
                window(
                    function,
                    function_id,
                    pc,
                    start,
                    usize::from(*number_of_elements),
                )?;
            }
        }
        Instruction::Close(start) => register(function, function_id, pc, *start)?,
        Instruction::Closure {
            destination,
            function: child,
        } => {
            register(function, function_id, pc, *destination)?;
            let child = function.closures.get(child.0 as usize).ok_or_else(|| {
                error(
                    function_id,
                    Some(pc),
                    "closure operand in bounds",
                    format!(
                        "child {} exceeds prototype count {}",
                        child.0,
                        function.closures.len()
                    ),
                )
            })?;
            for capture in 0..usize::from(child.number_of_upvalues) {
                let capture_pc = pc.checked_add(capture + 1).ok_or_else(|| {
                    error(
                        function_id,
                        Some(pc),
                        "closure capture stream",
                        "capture instruction index overflow",
                    )
                })?;
                match function.code.get(capture_pc) {
                    Some(Instruction::Move { source, .. }) => {
                        register(function, function_id, capture_pc, *source)?;
                    }
                    Some(Instruction::GetUpvalue {
                        upvalue: source, ..
                    }) => {
                        upvalue(function, function_id, capture_pc, source.0)?;
                    }
                    Some(_) => {
                        return Err(error(
                            function_id,
                            Some(capture_pc),
                            "closure capture descriptor",
                            "capture must be MOVE or GETUPVAL",
                        ));
                    }
                    None => {
                        return Err(error(
                            function_id,
                            Some(pc),
                            "closure capture stream",
                            "capture descriptors extend past function end",
                        ));
                    }
                }
            }
        }
        Instruction::VarArg(start, count) => {
            register(function, function_id, pc, *start)?;
            if function.vararg_flag & VARARG_ISVARARG == 0 {
                return Err(error(
                    function_id,
                    Some(pc),
                    "VARARG inside variadic function",
                    "non-variadic prototype contains VARARG",
                ));
            }
            if *count > 0 {
                window(function, function_id, pc, start.0, usize::from(*count - 1))?;
            }
        }
        Instruction::ExtraWord(_) => {}
    }
    Ok(())
}

fn register(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    value: Register,
) -> Result<(), DecompileError> {
    if value.0 < function.maximum_stack_size {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "register inside declared stack",
            format!(
                "register {} exceeds stack size {}",
                value.0, function.maximum_stack_size
            ),
        ))
    }
}

fn constant(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    value: Constant,
) -> Result<(), DecompileError> {
    if (value.0 as usize) < function.constants.len() {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "constant operand in bounds",
            format!(
                "constant {} exceeds table size {}",
                value.0,
                function.constants.len()
            ),
        ))
    }
}

fn string_constant(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    value: Constant,
) -> Result<(), DecompileError> {
    constant(function, function_id, pc, value)?;
    if matches!(function.constants[value.0 as usize], Value::String(_)) {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "global name is string",
            format!("constant {} is not a string", value.0),
        ))
    }
}

fn register_or_constant(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    value: &RegisterOrConstant,
) -> Result<(), DecompileError> {
    match value.0 {
        Either::Left(value) => register(function, function_id, pc, value),
        Either::Right(value) => constant(function, function_id, pc, value),
    }
}

fn upvalue(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    value: u8,
) -> Result<(), DecompileError> {
    if value < function.number_of_upvalues {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "upvalue operand in bounds",
            format!(
                "upvalue {value} exceeds count {}",
                function.number_of_upvalues
            ),
        ))
    }
}

fn window(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    start: u8,
    count: usize,
) -> Result<(), DecompileError> {
    let end = usize::from(start).checked_add(count).ok_or_else(|| {
        error(
            function_id,
            Some(pc),
            "register window",
            "register window overflow",
        )
    })?;
    if end <= usize::from(function.maximum_stack_size) {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "register window inside declared stack",
            format!(
                "window {start}..{end} exceeds stack size {}",
                function.maximum_stack_size
            ),
        ))
    }
}

fn jump(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
    skip: i32,
) -> Result<(), DecompileError> {
    let target = jump_target(pc, skip).ok_or_else(|| {
        error(
            function_id,
            Some(pc),
            "jump target in function",
            "jump target overflow",
        )
    })?;
    if target <= function.code.len() {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "jump target in function",
            format!(
                "jump target {target} exceeds code size {}",
                function.code.len()
            ),
        ))
    }
}

fn jump_target(pc: usize, skip: i32) -> Option<usize> {
    (pc + 1).checked_add_signed(skip as isize)
}

fn conditional_fallthrough(
    function: &Function<'_>,
    function_id: usize,
    pc: usize,
) -> Result<(), DecompileError> {
    if pc
        .checked_add(2)
        .is_some_and(|target| target <= function.code.len())
    {
        Ok(())
    } else {
        Err(error(
            function_id,
            Some(pc),
            "conditional fallthrough in function",
            "conditional requires two following control-flow positions",
        ))
    }
}

fn error(
    function_id: usize,
    instruction: Option<usize>,
    invariant: &'static str,
    detail: impl Into<String>,
) -> DecompileError {
    DecompileError::new(
        DecompilePhase::Validate,
        Some(function_id),
        instruction,
        invariant,
        detail,
    )
}

#[cfg(test)]
mod tests {
    use lua51_deserializer::{
        Function, Instruction, Value,
        argument::{Constant, Function as FunctionIndex, Register},
    };

    use super::function_tree;

    fn function(code: Vec<Instruction>, closures: Vec<Function<'static>>) -> Function<'static> {
        Function {
            name: b"",
            line_defined: 0,
            last_line_defined: 0,
            number_of_upvalues: 0,
            vararg_flag: 0,
            maximum_stack_size: 1,
            code,
            constants: Vec::new(),
            closures,
            positions: Vec::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            number_of_parameters: 0,
        }
    }

    #[test]
    fn repeated_child_references_are_budgeted_by_expanded_work() {
        let leaf = function(vec![Instruction::Return(Register(0), 1)], Vec::new());
        let middle = function(
            vec![
                Instruction::Closure {
                    destination: Register(0),
                    function: FunctionIndex(0),
                },
                Instruction::Closure {
                    destination: Register(0),
                    function: FunctionIndex(0),
                },
                Instruction::Return(Register(0), 1),
            ],
            vec![leaf],
        );
        let root = function(
            vec![
                Instruction::Closure {
                    destination: Register(0),
                    function: FunctionIndex(0),
                },
                Instruction::Closure {
                    destination: Register(0),
                    function: FunctionIndex(0),
                },
                Instruction::Return(Register(0), 1),
            ],
            vec![middle],
        );

        let error = function_tree(&root, 12).unwrap_err();

        assert_eq!(error.invariant, "bounded prototype expansion");
    }

    #[test]
    fn control_flow_cannot_enter_setlist_extension_data() {
        let root = function(
            vec![
                Instruction::SetList {
                    table: Register(0),
                    number_of_elements: 0,
                    block_number: 1,
                },
                Instruction::ExtraWord(1),
                Instruction::Return(Register(0), 1),
                Instruction::Jump(-3),
            ],
            Vec::new(),
        );

        let error = function_tree(&root, 64).unwrap_err();

        assert_eq!(
            error.invariant,
            "control flow targets executable instruction"
        );
    }

    #[test]
    fn rejects_invalid_legacy_vararg_flag_combinations() {
        for flag in [1, 4, 5, 8] {
            let mut root = function(vec![Instruction::Return(Register(0), 1)], Vec::new());
            root.vararg_flag = flag;

            let error = function_tree(&root, 64).unwrap_err();

            assert_eq!(error.invariant, "valid Lua 5.1 vararg flags");
        }
    }

    #[test]
    fn legacy_arg_register_must_fit_the_stack() {
        let mut root = function(vec![Instruction::Return(Register(0), 1)], Vec::new());
        root.vararg_flag = 7;
        root.number_of_parameters = 1;

        let error = function_tree(&root, 64).unwrap_err();

        assert_eq!(error.invariant, "legacy arg register fits declared stack");
    }

    #[test]
    fn open_results_cannot_survive_an_intervening_instruction() {
        let mut root = function(
            vec![
                Instruction::Call {
                    function: Register(0),
                    arguments: 1,
                    return_values: 0,
                },
                Instruction::LoadConstant {
                    destination: Register(0),
                    source: Constant(0),
                },
                Instruction::Return(Register(0), 0),
            ],
            Vec::new(),
        );
        root.constants.push(Value::Number(1.0));

        let error = function_tree(&root, 64).unwrap_err();

        assert_eq!(error.instruction, Some(1));
        assert_eq!(error.invariant, "open result consumed by next instruction");
    }

    #[test]
    fn tailcall_terminates_without_leaving_an_open_result() {
        let root = function(
            vec![Instruction::TailCall {
                function: Register(0),
                arguments: 1,
            }],
            Vec::new(),
        );

        assert!(function_tree(&root, 64).is_ok());
    }

    #[test]
    fn tailcall_accepts_the_compiler_return_epilogue() {
        let root = function(
            vec![
                Instruction::TailCall {
                    function: Register(0),
                    arguments: 1,
                },
                Instruction::Return(Register(0), 0),
            ],
            Vec::new(),
        );

        assert!(function_tree(&root, 64).is_ok());
    }

    #[test]
    fn testset_requires_its_jump_successor() {
        let root = function(
            vec![
                Instruction::TestSet {
                    destination: Register(0),
                    value: Register(0),
                    invert: false,
                },
                Instruction::Return(Register(0), 1),
            ],
            Vec::new(),
        );

        let error = function_tree(&root, 64).unwrap_err();

        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.invariant, "TESTSET followed by jump");
    }
}
