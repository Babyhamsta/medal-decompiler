use indexmap::IndexSet;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Block, Call, Function, LValue, LocalRw, RValue, RcLocal, Statement, Traverse,
    is_valid_identifier,
};

#[derive(Default)]
struct Evidence {
    reads: usize,
    roles: FxHashSet<&'static str>,
    table_initializer: bool,
    returned: bool,
    dynamic_callback_fields: usize,
    callback_source: bool,
    direct_calls: usize,
    generic_value: bool,
    field_names: FxHashSet<String>,
    /// The name suggested by the initializer's form, used only when no
    /// stronger evidence names this local.
    shape: Option<&'static str>,
    /// Written through a computed index anywhere in the enclosing scope. Any
    /// such write disqualifies the generic `result` role: a table mutated
    /// through a computed key is doing more than being handed back verbatim.
    computed_index_writes: usize,
    /// Written through a computed index specifically inside a loop. Only
    /// this — not a single write anywhere — justifies promoting an empty
    /// table's shape to `registers` (array/register-file semantics). A lone
    /// `t[hash(key)] = v` outside a loop is a cache or hash table, not an
    /// array being filled, and asserting `registers` for it would be a wrong
    /// name, not just a generic one.
    computed_index_writes_in_loop: usize,
    /// Written through a string-literal key (`t.field = x`), the signal that
    /// an initially empty table is a record with fixed fields rather than
    /// array storage.
    constant_index_writes: usize,
}

impl Evidence {
    fn role(&self) -> Option<&'static str> {
        (self.roles.len() == 1).then(|| *self.roles.iter().next().unwrap())
    }

    fn field_name(&self) -> Option<&String> {
        (self.field_names.len() == 1).then(|| self.field_names.iter().next().unwrap())
    }
}

/// Names for values whose producing call fixes their meaning.
///
/// Only entries whose name is certain belong here. A wrong name is worse
/// than a generic one, because it asserts something false about the code.
const LIBRARY_RETURN_NAMES: &[(&[u8], &[u8], &str)] = &[
    (b"table", b"pack", "packed"),
    // Not "buffer": Luau has a genuine native `buffer` type and `buffer.*`
    // library. A `table.create(n)` array is unrelated to it — this is
    // exactly the same shape as an empty `{}` initializer (see the `slots`
    // arm below), just sized up front, so it gets the same name for the
    // same reason instead of a name that would misread as the other type.
    (b"table", b"create", "slots"),
    (b"table", b"concat", "text"),
    (b"string", b"format", "text"),
    (b"string", b"rep", "text"),
    (b"coroutine", b"create", "thread"),
    // Not "started": os.clock() is a monotonic timestamp with no inherent
    // direction. It is read as an end marker and fed straight into a
    // subtraction just as often as it opens a measurement, so "started"
    // would assert a fact the call itself does not establish. "timestamp"
    // is true at every call site.
    (b"os", b"clock", "timestamp"),
];

/// Names a call's result from the specific library function that produced
/// it. Matches on the global's name alone, not its origin: real-world
/// bytecode (hardened or reconstructed scripts especially) often reaches
/// `table.pack` through plain `GETGLOBAL`/`GETTABLEKS` rather than the
/// compiler's `GETIMPORT` encoding, so restricting to
/// `GlobalOrigin::CompilerImport` misses the majority of these calls.
fn library_return_name(value: &RValue) -> Option<&'static str> {
    let call = match value {
        RValue::Call(call) | RValue::Select(crate::Select::Call(call)) => call,
        _ => return None,
    };
    match call.value.as_ref() {
        RValue::Index(index) => {
            let RValue::Global(namespace) = index.left.as_ref() else {
                return None;
            };
            let RValue::Literal(crate::Literal::String(member)) = index.right.as_ref() else {
                return None;
            };
            LIBRARY_RETURN_NAMES
                .iter()
                .find(|(space, name, _)| {
                    *space == namespace.name() && name == &member.as_slice()
                })
                .map(|(_, _, label)| *label)
        }
        RValue::Global(global) if global.name() == b"setmetatable" => Some("object"),
        RValue::Global(global) if global.name() == b"pcall" => Some("ok"),
        _ => None,
    }
}

/// Arithmetic operators plain enough that, on their own with no length
/// operand, they carry no more information than the counter they'd
/// otherwise be named with — see `initializer_shape`'s `Binary` arm.
fn is_plain_arithmetic(operation: crate::BinaryOperation) -> bool {
    matches!(
        operation,
        crate::BinaryOperation::Add
            | crate::BinaryOperation::Sub
            | crate::BinaryOperation::Mul
            | crate::BinaryOperation::Div
            | crate::BinaryOperation::Mod
            | crate::BinaryOperation::Pow
            | crate::BinaryOperation::IDiv
    )
}

/// True for a bare `#x`, or arithmetic combining one, such as `#offsets - 1`.
/// False for arithmetic that never touches a length, such as `last - first`:
/// that expression is not "nothing inferable" (it is visibly a difference of
/// two named values) but it is not a count either, and guessing wrong here
/// would assert something false about the code.
fn is_count_expression(value: &RValue) -> bool {
    match value {
        RValue::Unary(unary) => matches!(unary.operation, crate::UnaryOperation::Length),
        RValue::Binary(binary) if is_plain_arithmetic(binary.operation) => {
            is_count_expression(&binary.left) || is_count_expression(&binary.right)
        }
        _ => false,
    }
}

fn initializer_shape(value: &RValue) -> Option<&'static str> {
    match value {
        RValue::Closure(_) => Some("handler"),
        RValue::Literal(crate::Literal::String(_)) => Some("text"),
        // A bare number/boolean/nil literal is already visible at the
        // declaration site (`local value = 0` says no more than `local v2 =
        // 0`), so it is deliberately left unclassified rather than forced
        // into the catch-all below.
        RValue::Literal(_) => None,
        RValue::Unary(unary) if matches!(unary.operation, crate::UnaryOperation::Length) => {
            Some("count")
        }
        RValue::Binary(binary) => {
            if matches!(binary.operation, crate::BinaryOperation::Concat) {
                Some("text")
            } else if is_count_expression(value) {
                Some("count")
            } else if is_plain_arithmetic(binary.operation) {
                // Outside both taxonomy rows: not a count (no length
                // operand) and not "nothing inferable" either (it is
                // visibly arithmetic on named operands). Left unclassified
                // rather than mislabeled `value`.
                None
            } else {
                // Comparisons and `and`/`or` short-circuits: genuinely
                // nothing more specific is inferable.
                Some("value")
            }
        }
        RValue::Table(table) if table.0.is_empty() => Some("slots"),
        RValue::Table(table) => {
            let all_string_keys = table.0.iter().all(|(key, _)| {
                matches!(key, Some(RValue::Literal(crate::Literal::String(_))))
            });
            Some(if all_string_keys { "record" } else { "slots" })
        }
        RValue::Call(_) | RValue::Select(crate::Select::Call(_)) => {
            Some(library_return_name(value).unwrap_or("result"))
        }
        RValue::MethodCall(_) => Some("result"),
        // Genuine catch-all: an index/global read, a bare local alias, a
        // conditional, a vararg, or anything else with no more specific
        // shape is still visibly a value, and naming it that is honest.
        _ => Some("value"),
    }
}

struct Namer {
    rename: bool,
    counter: usize,
    /// One frame per enclosing function. A name is taken if any frame holds
    /// it, so an inner binding cannot shadow one that is still visible.
    scopes: Vec<FxHashSet<String>>,
}

impl Namer {
    fn add_declarations(block: &Block, declarations: &mut IndexSet<RcLocal>) {
        for statement in &block.0 {
            match statement {
                Statement::Assign(assign) if assign.prefix => {
                    declarations.extend(assign.left.iter().filter_map(LValue::as_local).cloned());
                }
                Statement::Class(class) => {
                    declarations.insert(class.target.clone());
                }
                Statement::NumericFor(numeric_for) => {
                    declarations.insert(numeric_for.counter.clone());
                    Self::add_declarations(&numeric_for.block.lock(), declarations);
                }
                Statement::GenericFor(generic_for) => {
                    declarations.extend(generic_for.res_locals.iter().cloned());
                    Self::add_declarations(&generic_for.block.lock(), declarations);
                }
                Statement::If(r#if) => {
                    Self::add_declarations(&r#if.then_block.lock(), declarations);
                    Self::add_declarations(&r#if.else_block.lock(), declarations);
                }
                Statement::While(r#while) => {
                    Self::add_declarations(&r#while.block.lock(), declarations);
                }
                Statement::Repeat(repeat) => {
                    Self::add_declarations(&repeat.block.lock(), declarations);
                }
                _ => {}
            }
        }
    }

    fn record_call(
        call: &Call,
        parameters: &FxHashSet<RcLocal>,
        evidence: &mut FxHashMap<RcLocal, Evidence>,
    ) {
        if let RValue::Local(local) = call.value.as_ref() {
            let local_evidence = evidence.entry(local.clone()).or_default();
            local_evidence.direct_calls += 1;
            if parameters.contains(local) {
                local_evidence.roles.insert("callback");
            }
        }
    }

    fn is_callback_source(value: &RValue) -> bool {
        match value {
            RValue::Index(_) => true,
            RValue::Binary(binary)
                if matches!(
                    binary.operation,
                    crate::BinaryOperation::And | crate::BinaryOperation::Or
                ) =>
            {
                Self::is_callback_source(&binary.left) || Self::is_callback_source(&binary.right)
            }
            RValue::Conditional(conditional) => {
                Self::is_callback_source(&conditional.then_value)
                    || Self::is_callback_source(&conditional.else_value)
            }
            _ => false,
        }
    }

    fn record_rvalue(
        value: &RValue,
        parameters: &FxHashSet<RcLocal>,
        evidence: &mut FxHashMap<RcLocal, Evidence>,
    ) {
        match value {
            RValue::Call(call) | RValue::Select(crate::Select::Call(call)) => {
                Self::record_call(call, parameters, evidence);
            }
            RValue::Table(table) => {
                for (key, value) in &table.0 {
                    let Some(RValue::Literal(crate::Literal::String(field))) = key else {
                        continue;
                    };
                    let Ok(field) = std::str::from_utf8(field) else {
                        continue;
                    };
                    if !is_valid_identifier(field.as_bytes()) {
                        continue;
                    }
                    if let RValue::Local(local) = value {
                        evidence
                            .entry(local.clone())
                            .or_default()
                            .field_names
                            .insert(field.to_owned());
                    }
                }
            }
            _ => {}
        }
        for child in value.rvalues() {
            Self::record_rvalue(child, parameters, evidence);
        }
    }

    fn iterator_name(generic_for: &crate::GenericFor) -> Option<&[u8]> {
        let call = match generic_for.right.first()? {
            RValue::Call(call) => call,
            RValue::Select(crate::Select::Call(call)) => call,
            _ => return None,
        };
        let RValue::Global(global) = call.value.as_ref() else {
            return None;
        };
        (global.origin() == crate::GlobalOrigin::CompilerImport).then(|| global.name())
    }

    fn collect_evidence(
        block: &Block,
        parameters: &FxHashSet<RcLocal>,
        evidence: &mut FxHashMap<RcLocal, Evidence>,
        structural_names: &mut FxHashMap<RcLocal, String>,
        in_loop: bool,
    ) {
        for statement in &block.0 {
            for local in statement.values_read() {
                evidence.entry(local.clone()).or_default().reads += 1;
            }
            for value in statement.rvalues() {
                Self::record_rvalue(value, parameters, evidence);
            }

            match statement {
                Statement::Call(call) => Self::record_call(call, parameters, evidence),
                Statement::Assign(assign) => {
                    if assign.prefix
                        && let ([LValue::Local(local)], [value]) =
                            (assign.left.as_slice(), assign.right.as_slice())
                    {
                        if matches!(value, RValue::Table(_)) {
                            evidence.entry(local.clone()).or_default().table_initializer = true;
                        }
                        if Self::is_callback_source(value) {
                            evidence.entry(local.clone()).or_default().callback_source = true;
                        }
                        if let RValue::Closure(closure) = value
                            && let Some(name) = closure.function.lock().name.clone()
                            && is_valid_identifier(name.as_bytes())
                        {
                            structural_names.entry(local.clone()).or_insert(name);
                        }
                        if let Some(shape) = initializer_shape(value) {
                            evidence.entry(local.clone()).or_default().shape = Some(shape);
                        }
                    }

                    if let [LValue::Index(index)] = assign.left.as_slice()
                        && let RValue::Local(container) = index.left.as_ref()
                    {
                        let container_evidence = evidence.entry(container.clone()).or_default();
                        match index.right.as_ref() {
                            // `t.field = x`: names a fixed field, the
                            // record signal.
                            RValue::Literal(crate::Literal::String(_)) => {
                                container_evidence.constant_index_writes += 1;
                            }
                            // `t[1] = x`: a positional literal key asserts
                            // neither array-building nor fixed-field intent
                            // on its own, so it moves neither counter.
                            RValue::Literal(_) => {}
                            _ => {
                                container_evidence.computed_index_writes += 1;
                                if in_loop {
                                    container_evidence.computed_index_writes_in_loop += 1;
                                }
                            }
                        }
                    }

                    if let ([LValue::Index(index)], [RValue::Closure(_)]) =
                        (assign.left.as_slice(), assign.right.as_slice())
                        && let RValue::Local(container) = index.left.as_ref()
                        && !matches!(
                            index.right.as_ref(),
                            RValue::Literal(crate::Literal::String(_))
                        )
                    {
                        evidence
                            .entry(container.clone())
                            .or_default()
                            .dynamic_callback_fields += 1;
                    }
                }
                Statement::Class(class) => {
                    if is_valid_identifier(class.source_name.as_bytes()) {
                        structural_names
                            .entry(class.target.clone())
                            .or_insert_with(|| class.source_name.clone());
                    }
                }
                Statement::Return(r#return) => {
                    for value in &r#return.values {
                        if let RValue::Local(local) = value {
                            evidence.entry(local.clone()).or_default().returned = true;
                        }
                    }
                }
                Statement::NumericFor(numeric_for) => {
                    evidence
                        .entry(numeric_for.counter.clone())
                        .or_default()
                        .roles
                        .insert("index");
                    Self::collect_evidence(
                        &numeric_for.block.lock(),
                        parameters,
                        evidence,
                        structural_names,
                        true,
                    );
                }
                Statement::GenericFor(generic_for) => {
                    match (
                        Self::iterator_name(generic_for),
                        generic_for.res_locals.as_slice(),
                    ) {
                        (Some(b"pairs"), [key, value, ..]) => {
                            evidence.entry(key.clone()).or_default().roles.insert("key");
                            let value_evidence = evidence.entry(value.clone()).or_default();
                            value_evidence.roles.insert("value");
                            value_evidence.generic_value = true;
                        }
                        (Some(b"ipairs"), [index, entry, ..]) => {
                            evidence
                                .entry(index.clone())
                                .or_default()
                                .roles
                                .insert("index");
                            let entry_evidence = evidence.entry(entry.clone()).or_default();
                            entry_evidence.roles.insert("entry");
                            entry_evidence.generic_value = true;
                        }
                        _ => {
                            for value in &generic_for.res_locals {
                                evidence.entry(value.clone()).or_default().generic_value = true;
                            }
                        }
                    }
                    Self::collect_evidence(
                        &generic_for.block.lock(),
                        parameters,
                        evidence,
                        structural_names,
                        true,
                    );
                }
                Statement::If(r#if) => {
                    Self::collect_evidence(
                        &r#if.then_block.lock(),
                        parameters,
                        evidence,
                        structural_names,
                        in_loop,
                    );
                    Self::collect_evidence(
                        &r#if.else_block.lock(),
                        parameters,
                        evidence,
                        structural_names,
                        in_loop,
                    );
                }
                Statement::While(r#while) => Self::collect_evidence(
                    &r#while.block.lock(),
                    parameters,
                    evidence,
                    structural_names,
                    true,
                ),
                Statement::Repeat(repeat) => Self::collect_evidence(
                    &repeat.block.lock(),
                    parameters,
                    evidence,
                    structural_names,
                    true,
                ),
                _ => {}
            }
        }
    }

    fn is_taken(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.contains(name))
    }

    fn claim(&mut self, name: String) -> bool {
        if self.is_taken(&name) {
            return false;
        }
        self.scopes
            .last_mut()
            .expect("namer always has an open scope")
            .insert(name);
        true
    }

    fn unique_name(&mut self, base: &str) -> String {
        if self.claim(base.to_owned()) {
            return base.to_owned();
        }
        for suffix in 2.. {
            let name = format!("{base}{suffix}");
            if self.claim(name.clone()) {
                return name;
            }
        }
        unreachable!()
    }

    fn fallback_name(&mut self, prefix: &str) -> String {
        loop {
            let name = format!("{prefix}{}", self.counter);
            self.counter += 1;
            if self.claim(name.clone()) {
                return name;
            }
        }
    }

    fn assign_name(
        &mut self,
        local: &RcLocal,
        prefix: &str,
        evidence: Option<&Evidence>,
        structural_name: Option<&String>,
    ) {
        let existing = (!self.rename)
            .then(|| local.0.0.lock().0.clone())
            .flatten()
            .filter(|name| is_valid_identifier(name.as_bytes()));
        let inferred = evidence.and_then(Evidence::role);
        let field_name = evidence.and_then(Evidence::field_name).cloned();
        let unused = evidence.is_none_or(|evidence| evidence.reads == 0);
        let shape = evidence.and_then(|evidence| evidence.shape);

        let name = if let Some(name) = existing.or_else(|| structural_name.cloned()).or(field_name)
        {
            self.unique_name(&name)
        } else if let Some(role) = inferred {
            self.unique_name(role)
        } else if unused {
            "_".to_owned()
        } else if let Some(shape) = shape {
            self.unique_name(shape)
        } else {
            self.fallback_name(prefix)
        };
        local.0.0.lock().0 = Some(name);
    }

    fn name_scope(&mut self, block: &mut Block, parameters: &[RcLocal]) {
        let mut declarations = parameters.iter().cloned().collect::<IndexSet<_>>();
        Self::add_declarations(block, &mut declarations);
        let parameter_set = parameters.iter().cloned().collect::<FxHashSet<_>>();
        let mut evidence = FxHashMap::default();
        let mut structural_names = FxHashMap::default();
        Self::collect_evidence(
            block,
            &parameter_set,
            &mut evidence,
            &mut structural_names,
            false,
        );
        for evidence in evidence.values_mut() {
            if evidence.table_initializer
                && evidence.returned
                && evidence.computed_index_writes == 0
                && evidence.constant_index_writes == 0
            {
                evidence.roles.insert("result");
            }
            if evidence.dynamic_callback_fields >= 2 {
                evidence.roles.insert("callbacks");
            }
            if evidence.callback_source && evidence.direct_calls > 0 {
                evidence.roles.insert("callback");
            }
            if evidence.generic_value && evidence.direct_calls > 0 {
                evidence.roles.clear();
                evidence.roles.insert("callback");
            }
            if evidence.shape == Some("slots") && evidence.constant_index_writes > 0 {
                evidence.shape = Some("record");
            }
            // Loop-scoped, not just "anywhere": a single computed-index
            // write outside a loop is a cache or hash table, and asserting
            // array/register-file semantics for it would be a wrong name,
            // not just a generic one.
            if evidence.shape == Some("slots") && evidence.computed_index_writes_in_loop > 0 {
                evidence.shape = Some("registers");
            }
        }

        for parameter in parameters {
            self.assign_name(
                parameter,
                "p",
                evidence.get(parameter),
                structural_names.get(parameter),
            );
        }
        for local in declarations
            .iter()
            .filter(|local| !parameter_set.contains(*local))
        {
            self.assign_name(local, "v", evidence.get(local), structural_names.get(local));
        }

        self.name_child_functions(block);
    }

    fn name_function(&mut self, function: &mut Function) {
        let parameters = function.parameters.clone();
        self.name_scope(&mut function.body, &parameters);
    }

    fn name_closure(&mut self, closure: &crate::Closure) {
        let upvalue_names = closure
            .upvalues
            .iter()
            .filter_map(|upvalue| {
                let local = match upvalue {
                    crate::Upvalue::Copy(local) | crate::Upvalue::Ref(local) => local,
                };
                local
                    .0
                    .0
                    .lock()
                    .0
                    .clone()
                    .filter(|name| is_valid_identifier(name.as_bytes()))
            })
            .collect::<FxHashSet<_>>();
        let mut function = closure.function.lock();
        if function.is_method && self.is_taken("self") {
            function.is_method = false;
        }

        let outer_counter = std::mem::replace(&mut self.counter, 1);
        self.scopes.push(upvalue_names);
        self.name_function(&mut function);
        self.scopes.pop();
        self.counter = outer_counter;
    }

    fn name_child_functions(&mut self, block: &mut Block) {
        for statement in &mut block.0 {
            let mut children = Vec::new();
            statement.traverse_rvalues(&mut |value| {
                if let RValue::Closure(closure) = value {
                    children.push(closure.clone());
                }
            });
            for closure in &children {
                self.name_closure(closure);
            }
            match statement {
                Statement::If(r#if) => {
                    self.name_child_functions(&mut r#if.then_block.lock());
                    self.name_child_functions(&mut r#if.else_block.lock());
                }
                Statement::While(r#while) => {
                    self.name_child_functions(&mut r#while.block.lock());
                }
                Statement::Repeat(repeat) => {
                    self.name_child_functions(&mut repeat.block.lock());
                }
                Statement::NumericFor(numeric_for) => {
                    self.name_child_functions(&mut numeric_for.block.lock());
                }
                Statement::GenericFor(generic_for) => {
                    self.name_child_functions(&mut generic_for.block.lock());
                }
                _ => {}
            }
        }
    }
}

pub fn name_locals(block: &mut Block, rename: bool) {
    Namer {
        rename,
        counter: 1,
        scopes: vec![FxHashSet::default()],
    }
    .name_scope(block, &[]);
}

#[cfg(test)]
mod tests {
    use by_address::ByAddress;
    use parking_lot::Mutex;
    use triomphe::Arc;

    use crate::{
        Assign, Block, Call, Closure, Function, GenericFor, Global, LValue, Literal, Local,
        NumericFor, RValue, RcLocal, Return, Table, Upvalue,
    };

    use super::name_locals;

    fn local(name: Option<&str>) -> RcLocal {
        RcLocal::new(Local::new(name.map(str::to_owned)))
    }

    fn local_name(local: &RcLocal) -> String {
        local.0.0.lock().0.clone().unwrap()
    }

    fn declaration(local: &RcLocal, value: RValue) -> Assign {
        let mut assign = Assign::new(vec![LValue::Local(local.clone())], vec![value]);
        assign.prefix = true;
        assign
    }

    #[test]
    fn infers_numeric_and_generic_for_roles() {
        let numeric_index = local(None);
        let pair_key = local(None);
        let pair_value = local(None);
        let ipairs_index = local(None);
        let entry = local(None);
        let invoked_index = local(None);
        let invoked_callback = local(None);
        let unknown_value = local(None);
        let values = local(Some("values"));
        let pairs = Call::new(
            Global::compiler_import(b"pairs".to_vec()).into(),
            vec![values.clone().into()],
        );
        let ipairs = Call::new(
            Global::compiler_import(b"ipairs".to_vec()).into(),
            vec![values.into()],
        );
        let mut block = Block(vec![
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(3.0).into(),
                Literal::Number(1.0).into(),
                numeric_index.clone(),
                Block::default(),
            )
            .into(),
            GenericFor::new(
                vec![pair_key.clone(), pair_value.clone()],
                vec![pairs.into()],
                Block::default(),
            )
            .into(),
            GenericFor::new(
                vec![ipairs_index.clone(), entry.clone()],
                vec![ipairs.clone().into()],
                Block::default(),
            )
            .into(),
            GenericFor::new(
                vec![invoked_index.clone(), invoked_callback.clone()],
                vec![ipairs.into()],
                Block(vec![
                    Call::new(invoked_callback.clone().into(), Vec::new()).into(),
                ]),
            )
            .into(),
            GenericFor::new(
                vec![unknown_value.clone()],
                vec![Global::from("iterate").into()],
                Block::default(),
            )
            .into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&numeric_index), "index");
        assert_eq!(local_name(&pair_key), "key");
        assert_eq!(local_name(&pair_value), "value");
        assert_eq!(local_name(&ipairs_index), "index2");
        assert_eq!(local_name(&entry), "entry");
        assert_eq!(local_name(&invoked_index), "index3");
        assert_eq!(local_name(&invoked_callback), "callback");
        assert_ne!(local_name(&unknown_value), "entry");
        assert_ne!(local_name(&unknown_value), "value");
    }

    #[test]
    fn infers_returned_table_and_invoked_parameter_roles() {
        let result = local(None);
        let callback = local(None);
        let function = Arc::new(Mutex::new(Function {
            parameters: vec![callback.clone()],
            body: Block(vec![
                Call::new(callback.clone().into(), Vec::new()).into(),
                Return::new(Vec::new()).into(),
            ]),
            ..Function::default()
        }));
        let closure = Closure {
            function: ByAddress(function),
            upvalues: Vec::new(),
        };
        let worker = local(None);
        let mut block = Block(vec![
            declaration(&result, Table::default().into()).into(),
            declaration(&worker, closure.into()).into(),
            Return::new(vec![result.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&result), "result");
        assert_eq!(local_name(&callback), "callback");
    }

    #[test]
    fn keeps_valid_debug_names_but_rejects_collisions_and_keywords() {
        let first = local(Some("value"));
        let duplicate = local(Some("value"));
        let keyword = local(Some("end"));
        let mut block = Block(vec![
            declaration(&first, Literal::Nil.into()).into(),
            declaration(&duplicate, Literal::Nil.into()).into(),
            declaration(&keyword, Literal::Nil.into()).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&first), "value");
        assert_ne!(local_name(&duplicate), "value");
        assert_ne!(local_name(&keyword), "end");
    }

    #[test]
    fn generated_fallback_does_not_expose_upvalue_marker() {
        let captured = local(None);
        let closure_local = local(None);
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function::default()))),
            upvalues: vec![Upvalue::Ref(captured.clone())],
        };
        let mut block = Block(vec![
            declaration(&captured, Literal::Nil.into()).into(),
            declaration(&closure_local, closure.into()).into(),
        ]);

        name_locals(&mut block, false);

        assert!(!local_name(&captured).contains("_u_"));
    }

    #[test]
    fn sibling_methods_can_each_keep_self() {
        let first_receiver = local(Some("self"));
        let second_receiver = local(Some("self"));
        let method = |receiver: RcLocal| Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                parameters: vec![receiver.clone()],
                is_method: true,
                body: Block(vec![Return::new(vec![receiver.into()]).into()]),
                ..Function::default()
            }))),
            upvalues: Vec::new(),
        };
        let controller = Global::from("Controller");
        let mut block = Block(vec![
            Assign::new(
                vec![
                    crate::Index::new(
                        controller.clone().into(),
                        Literal::String(b"first".to_vec()).into(),
                    )
                    .into(),
                ],
                vec![method(first_receiver.clone()).into()],
            )
            .into(),
            Assign::new(
                vec![
                    crate::Index::new(
                        controller.into(),
                        Literal::String(b"second".to_vec()).into(),
                    )
                    .into(),
                ],
                vec![method(second_receiver.clone()).into()],
            )
            .into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&first_receiver), "self");
        assert_eq!(local_name(&second_receiver), "self");
    }

    #[test]
    fn captured_outer_self_keeps_recovered_function_semantically_bound() {
        let outer_self = local(Some("self"));
        let receiver = local(Some("self"));
        let function = Arc::new(Mutex::new(Function {
            parameters: vec![receiver.clone()],
            is_method: true,
            body: Block(vec![Return::new(vec![receiver.clone().into()]).into()]),
            ..Function::default()
        }));
        let closure = Closure {
            function: ByAddress(function.clone()),
            upvalues: vec![Upvalue::Copy(outer_self.clone())],
        };
        let mut block = Block(vec![
            declaration(&outer_self, Table::default().into()).into(),
            Assign::new(
                vec![
                    crate::Index::new(
                        Global::from("Controller").into(),
                        Literal::String(b"method".to_vec()).into(),
                    )
                    .into(),
                ],
                vec![closure.into()],
            )
            .into(),
        ]);

        name_locals(&mut block, false);

        assert!(!function.lock().is_method);
        assert_ne!(local_name(&receiver), "self");
    }

    #[test]
    fn infers_callback_collection_from_independent_fields() {
        let callbacks = local(None);
        let start_key = local(Some("startKey"));
        let stop_key = local(Some("stopKey"));
        let closure = || Closure {
            function: ByAddress(Arc::new(Mutex::new(Function::default()))),
            upvalues: Vec::new(),
        };
        let field =
            |key: &RcLocal| crate::Index::new(callbacks.clone().into(), key.clone().into()).into();
        let mut block = Block(vec![
            declaration(&callbacks, Table::default().into()).into(),
            Assign::new(vec![field(&start_key)], vec![RValue::Closure(closure())]).into(),
            Assign::new(vec![field(&stop_key)], vec![RValue::Closure(closure())]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&callbacks), "callbacks");
    }

    #[test]
    fn does_not_misname_static_method_table_as_callbacks() {
        let class_table = local(None);
        let closure = || Closure {
            function: ByAddress(Arc::new(Mutex::new(Function::default()))),
            upvalues: Vec::new(),
        };
        let field = |name: &[u8]| {
            crate::Index::new(
                class_table.clone().into(),
                Literal::String(name.to_vec()).into(),
            )
            .into()
        };
        let mut block = Block(vec![
            declaration(&class_table, Table::default().into()).into(),
            Assign::new(vec![field(b"new")], vec![RValue::Closure(closure())]).into(),
            Assign::new(vec![field(b"dispatch")], vec![RValue::Closure(closure())]).into(),
        ]);

        name_locals(&mut block, false);

        assert_ne!(local_name(&class_table), "callbacks");
    }

    #[test]
    fn infers_indexed_callable_but_not_local_helper_as_callback() {
        let registry = local(Some("registry"));
        let key = local(Some("key"));
        let indexed_callback = local(None);
        let helper = local(None);
        let helper_closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function::default()))),
            upvalues: Vec::new(),
        };
        let mut block = Block(vec![
            declaration(
                &indexed_callback,
                crate::Index::new(registry.into(), key.into()).into(),
            )
            .into(),
            declaration(&helper, helper_closure.into()).into(),
            Call::new(indexed_callback.clone().into(), Vec::new()).into(),
            Call::new(helper.clone().into(), Vec::new()).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&indexed_callback), "callback");
        assert_ne!(local_name(&helper), "callback");
    }

    #[test]
    fn inner_scope_never_shadows_a_visible_outer_name() {
        let outer = local(None);
        let inner = local(None);
        let mut inner_body = Block(vec![
            declaration(&inner, Literal::Number(2.0).into()).into(),
            crate::Return::new(vec![inner.clone().into()]).into(),
        ]);
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: Vec::new(),
                is_variadic: false,
                is_method: false,
                body: std::mem::take(&mut inner_body),
            }))),
            upvalues: Vec::new(),
        };
        let holder = local(None);
        let mut block = Block(vec![
            declaration(&outer, Literal::Number(1.0).into()).into(),
            declaration(&holder, closure.into()).into(),
            crate::Return::new(vec![outer.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_ne!(local_name(&outer), local_name(&inner));
    }

    #[test]
    fn sibling_scopes_may_reuse_a_name() {
        let first = local(None);
        let second = local(None);
        let mut left = Block(vec![declaration(&first, Literal::Number(1.0).into()).into()]);
        let mut right = Block(vec![declaration(&second, Literal::Number(2.0).into()).into()]);
        let make = |body: &mut Block| Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: Vec::new(),
                is_variadic: false,
                is_method: false,
                body: std::mem::take(body),
            }))),
            upvalues: Vec::new(),
        };
        let left_holder = local(None);
        let right_holder = local(None);
        let mut block = Block(vec![
            declaration(&left_holder, make(&mut left).into()).into(),
            declaration(&right_holder, make(&mut right).into()).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&first), local_name(&second));
    }

    #[test]
    fn table_initializer_indexed_in_a_loop_is_named_for_its_shape() {
        let registers = local(None);
        let counter = local(None);
        let body = Block(vec![
            Assign::new(
                vec![
                    crate::Index::new(registers.clone().into(), counter.clone().into()).into(),
                ],
                vec![Literal::Number(1.0).into()],
            )
            .into(),
        ]);
        let mut block = Block(vec![
            declaration(&registers, Table::default().into()).into(),
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(4.0).into(),
                Literal::Number(1.0).into(),
                counter,
                body,
            )
            .into(),
            Return::new(vec![registers.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&registers), "registers");
    }

    #[test]
    fn string_initializer_is_named_text() {
        let message = local(None);
        let mut block = Block(vec![
            declaration(&message, Literal::String(b"hello".to_vec()).into()).into(),
            Return::new(vec![message.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&message), "text");
    }

    #[test]
    fn table_pack_result_is_named_packed() {
        let packed = local(None);
        // `Global::new` (Dynamic origin) rather than `compiler_import`: hardened
        // and reconstructed bytecode commonly reaches `table.pack` through plain
        // GETGLOBAL/GETTABLEKS, so the match must not require the compiler's
        // GETIMPORT encoding.
        let call = Call::new(
            crate::Index::new(
                Global::new(b"table".to_vec()).into(),
                Literal::String(b"pack".to_vec()).into(),
            )
            .into(),
            Vec::new(),
        );
        let mut block = Block(vec![
            declaration(&packed, call.into()).into(),
            Return::new(vec![packed.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&packed), "packed");
    }

    #[test]
    fn library_call_without_a_lookup_entry_falls_back_to_result() {
        let outcome = local(None);
        let call = Call::new(
            crate::Index::new(
                Global::new(b"math".to_vec()).into(),
                Literal::String(b"random".to_vec()).into(),
            )
            .into(),
            Vec::new(),
        );
        let mut block = Block(vec![
            declaration(&outcome, call.into()).into(),
            Return::new(vec![outcome.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&outcome), "result");
    }

    #[test]
    fn dotted_call_on_a_lookalike_namespace_does_not_match() {
        let outcome = local(None);
        let call = Call::new(
            crate::Index::new(
                Global::new(b"notthetable".to_vec()).into(),
                Literal::String(b"pack".to_vec()).into(),
            )
            .into(),
            Vec::new(),
        );
        let mut block = Block(vec![
            declaration(&outcome, call.into()).into(),
            Return::new(vec![outcome.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&outcome), "result");
    }

    #[test]
    fn colon_method_call_named_pack_does_not_match() {
        let outcome = local(None);
        let receiver = local(Some("receiver"));
        let call = crate::MethodCall::new(receiver.into(), "pack".to_owned(), Vec::new());
        let mut block = Block(vec![
            declaration(&outcome, call.into()).into(),
            Return::new(vec![outcome.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&outcome), "result");
    }

    #[test]
    fn indexing_a_local_alias_of_table_does_not_match() {
        let outcome = local(None);
        let aliased_table = local(Some("t"));
        let call = Call::new(
            crate::Index::new(
                aliased_table.into(),
                Literal::String(b"pack".to_vec()).into(),
            )
            .into(),
            Vec::new(),
        );
        let mut block = Block(vec![
            declaration(&outcome, call.into()).into(),
            Return::new(vec![outcome.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&outcome), "result");
    }

    #[test]
    fn setmetatable_read_as_a_value_is_not_named_object() {
        let outcome = local(None);
        let mut block = Block(vec![
            declaration(&outcome, Global::new(b"setmetatable".to_vec()).into()).into(),
            Return::new(vec![outcome.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&outcome), "value");
    }

    #[test]
    fn a_local_with_no_inferable_shape_is_named_value_not_v1() {
        // A bare local-to-local alias: none of the specific patterns
        // (`Index`, `Global`, `Call`, ...) match it, so this only passes if
        // `value` is a genuine catch-all rather than a couple of hardcoded
        // cases.
        let unknown = local(None);
        let source = local(Some("source"));
        let mut block = Block(vec![
            declaration(&unknown, source.clone().into()).into(),
            Return::new(vec![unknown.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&unknown), "value");
    }

    #[test]
    fn or_default_initializer_with_no_other_evidence_is_named_value() {
        let unknown = local(None);
        let parameter = local(None);
        let mut block = Block(vec![
            declaration(
                &unknown,
                crate::Binary::new(
                    parameter.clone().into(),
                    Table::default().into(),
                    crate::BinaryOperation::Or,
                )
                .into(),
            )
            .into(),
            Return::new(vec![unknown.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&unknown), "value");
    }

    #[test]
    fn arithmetic_wrapping_a_length_is_named_count() {
        let offsets = local(Some("offsets"));
        let span = local(None);
        let mut block = Block(vec![
            declaration(
                &span,
                crate::Binary::new(
                    crate::Unary::new(offsets.into(), crate::UnaryOperation::Length).into(),
                    Literal::Number(1.0).into(),
                    crate::BinaryOperation::Sub,
                )
                .into(),
            )
            .into(),
            Return::new(vec![span.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&span), "count");
    }

    #[test]
    fn plain_arithmetic_with_no_length_operand_is_left_unclassified() {
        // `last - first`: visibly a computed difference, so it is not
        // "nothing inferable" either — naming it `value` or `count` would
        // both assert something the classifier cannot actually justify.
        let first = local(Some("first"));
        let last = local(Some("last"));
        let span = local(None);
        let mut block = Block(vec![
            declaration(
                &span,
                crate::Binary::new(
                    last.into(),
                    first.into(),
                    crate::BinaryOperation::Sub,
                )
                .into(),
            )
            .into(),
            Return::new(vec![span.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_ne!(local_name(&span), "count");
        assert_ne!(local_name(&span), "value");
    }

    #[test]
    fn single_computed_index_write_outside_a_loop_does_not_claim_registers() {
        // `t[hash(key)] = v; return t` — a cache or hash table, not an array
        // being filled. Promoting this to `registers` would assert
        // array/register-file semantics the code does not show.
        let cache = local(None);
        let key = local(None);
        let mut block = Block(vec![
            declaration(&cache, Table::default().into()).into(),
            Assign::new(
                vec![crate::Index::new(cache.clone().into(), key.into()).into()],
                vec![Literal::Number(1.0).into()],
            )
            .into(),
            Return::new(vec![cache.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_ne!(local_name(&cache), "registers");
    }

    #[test]
    fn empty_table_filled_with_named_fields_is_named_record() {
        let record = local(None);
        let mut block = Block(vec![
            declaration(&record, Table::default().into()).into(),
            Assign::new(
                vec![
                    crate::Index::new(
                        record.clone().into(),
                        Literal::String(b"name".to_vec()).into(),
                    )
                    .into(),
                ],
                vec![Literal::String(b"value".to_vec()).into()],
            )
            .into(),
            Return::new(vec![record.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&record), "record");
    }

    #[test]
    fn positional_literal_index_write_does_not_claim_record() {
        let list = local(None);
        let mut block = Block(vec![
            declaration(&list, Table::default().into()).into(),
            Assign::new(
                vec![crate::Index::new(list.clone().into(), Literal::Number(1.0).into()).into()],
                vec![Literal::Number(2.0).into()],
            )
            .into(),
            Return::new(vec![list.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_ne!(local_name(&list), "record");
    }
}
