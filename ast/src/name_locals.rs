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
    /// Written through a computed index, which distinguishes an array used as
    /// storage from a record with fixed fields.
    computed_index_writes: usize,
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

fn initializer_shape(value: &RValue) -> Option<&'static str> {
    match value {
        RValue::Closure(_) => Some("handler"),
        RValue::Literal(crate::Literal::String(_)) => Some("text"),
        RValue::Binary(binary) if matches!(binary.operation, crate::BinaryOperation::Concat) => {
            Some("text")
        }
        RValue::Unary(unary) if matches!(unary.operation, crate::UnaryOperation::Length) => {
            Some("count")
        }
        RValue::Table(table) if table.0.is_empty() => Some("slots"),
        RValue::Table(table) => {
            let all_string_keys = table.0.iter().all(|(key, _)| {
                matches!(key, Some(RValue::Literal(crate::Literal::String(_))))
            });
            Some(if all_string_keys { "record" } else { "slots" })
        }
        RValue::Call(_) | RValue::Select(crate::Select::Call(_)) => Some("result"),
        RValue::MethodCall(_) => Some("result"),
        RValue::Index(_) | RValue::Global(_) => Some("value"),
        _ => None,
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
                        if matches!(index.right.as_ref(), RValue::Literal(_)) {
                            container_evidence.constant_index_writes += 1;
                        } else {
                            container_evidence.computed_index_writes += 1;
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
                    );
                }
                Statement::If(r#if) => {
                    Self::collect_evidence(
                        &r#if.then_block.lock(),
                        parameters,
                        evidence,
                        structural_names,
                    );
                    Self::collect_evidence(
                        &r#if.else_block.lock(),
                        parameters,
                        evidence,
                        structural_names,
                    );
                }
                Statement::While(r#while) => Self::collect_evidence(
                    &r#while.block.lock(),
                    parameters,
                    evidence,
                    structural_names,
                ),
                Statement::Repeat(repeat) => Self::collect_evidence(
                    &repeat.block.lock(),
                    parameters,
                    evidence,
                    structural_names,
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
        Self::collect_evidence(block, &parameter_set, &mut evidence, &mut structural_names);
        for evidence in evidence.values_mut() {
            if evidence.table_initializer && evidence.returned && evidence.computed_index_writes == 0
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
            if evidence.shape == Some("slots") && evidence.computed_index_writes > 0 {
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
    fn a_local_with_no_inferable_shape_is_named_value_not_v1() {
        let unknown = local(None);
        let source = local(Some("source"));
        let mut block = Block(vec![
            declaration(
                &unknown,
                crate::Index::new(source.clone().into(), Literal::Number(1.0).into()).into(),
            )
            .into(),
            Return::new(vec![unknown.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&unknown), "value");
    }
}
